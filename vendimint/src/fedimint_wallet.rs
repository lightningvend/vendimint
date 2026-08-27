use std::{
    collections::{BTreeMap, HashMap},
    fmt::Display,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, anyhow, bail};
use bip39::Mnemonic;
use bitcoin::{
    Network, NetworkKind,
    bip32::{ChildNumber, Xpriv},
    hex::DisplayHex,
    secp256k1::{PublicKey, Secp256k1},
};
use fedimint_bip39::Bip39RootSecretStrategy;
use fedimint_client::{
    Client, ClientBuilder, ClientHandle, OperationId, RootSecret, secret::RootSecretStrategy,
};
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::{
    Amount,
    base32::{FEDIMINT_PREFIX, encode_prefixed},
    config::{ClientConfig, FederationId},
    db::Database,
    invite_code::InviteCode,
    util::SafeUrl,
};
use fedimint_lnv2_common::{Bolt11InvoiceDescription, ContractId};
use fedimint_lnv2_remote_client::{
    ClaimableContract, FinalRemoteReceiveOperationState, LightningClientModule,
    LightningRemoteClientInit,
};
use fedimint_mint_client::{
    MintClientInit as MintV1ClientInit, MintClientModule as MintV1ClientModule,
    SelectNotesWithExactAmount,
};
use fedimint_mintv2_client::{
    MintClientInit as MintV2ClientInit, MintClientModule as MintV2ClientModule,
};
use fedimint_rocksdb::RocksDb;
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, mpsc, oneshot, watch};

const WALLET_VIEW_UPDATE_INTERVAL: Duration = Duration::from_secs(5);

const MNEMONIC_PATH: &str = "mnemonic.txt";
const MNEMONIC_PASSWORD: &str = "";
const DEFAULT_FEDERATION_PATH: &str = "default_federation.txt";
const MINT_SELECTION_SUFFIX: &str = ".mint-module.json";
const LNV2_KIND: &str = "lnv2";
const MINT_V1_KIND: &str = "mint";
const MINT_V2_KIND: &str = "mintv2";

/// The e-cash module selected for a federation-backed wallet.
///
/// Vendimint selects exactly one generation per federation. New joins prefer
/// mint v2, while an existing wallet remains pinned to its original selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MintVersion {
    #[serde(rename = "mint")]
    V1,
    #[serde(rename = "mintv2")]
    V2,
}

impl MintVersion {
    const fn module_kind(self) -> &'static str {
        match self {
            Self::V1 => MINT_V1_KIND,
            Self::V2 => MINT_V2_KIND,
        }
    }
}

impl Display for MintVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.module_kind())
    }
}

/// An encoded e-cash export produced by either supported mint generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcashExport {
    token: String,
    amount: Amount,
    mint_version: MintVersion,
    reclaims_automatically: bool,
}

impl EcashExport {
    #[must_use]
    pub const fn total_amount(&self) -> Amount {
        self.amount
    }

    #[must_use]
    pub const fn mint_version(&self) -> MintVersion {
        self.mint_version
    }

    /// Whether the mint client will reclaim this export after its requested
    /// timeout if another client has not redeemed it.
    #[must_use]
    pub const fn reclaims_automatically(&self) -> bool {
        self.reclaims_automatically
    }
}

impl Display for EcashExport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.token)
    }
}

struct FederationClient {
    client: ClientHandle,
    mint_version: MintVersion,
}

type FederationClients = HashMap<FederationId, FederationClient>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletView {
    pub federations: BTreeMap<FederationId, FederationView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationView {
    pub federation_id: FederationId,
    pub name: Option<String>,
    pub balance: Amount,
    pub mint_version: MintVersion,
}

impl Display for FederationView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name_or_id = self
            .name
            .clone()
            .unwrap_or_else(|| self.federation_id.to_string());

        let balance = format_amount(self.balance);

        write!(f, "{name_or_id} ({balance})")
    }
}

fn format_amount(amount: Amount) -> String {
    let amount_sats = amount.msats / 1000;
    let sub_sat_msats = amount.msats % 1000;

    if amount_sats == 1 && sub_sat_msats == 0 {
        return "1 sat".to_string();
    }

    let comma_formatted_sats = amount_sats
        .to_string()
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(std::str::from_utf8)
        .collect::<Result<Vec<&str>, _>>()
        .unwrap()
        .join(",");

    let msats_str = if sub_sat_msats == 0 {
        String::new()
    } else {
        let mut sub_sat_msats_str = format!(".{sub_sat_msats:03}");
        while sub_sat_msats_str.ends_with('0') {
            sub_sat_msats_str.pop();
        }
        sub_sat_msats_str
    };

    format!("{comma_formatted_sats}{msats_str} sats")
}

pub struct Wallet {
    root_secret: RootSecret,
    clients: Arc<RwLock<FederationClients>>,
    fedimint_clients_data_dir: Mutex<PathBuf>,
    view_update_receiver: watch::Receiver<WalletView>,
    // Used to tell `Self.view_update_task` to immediately update the view.
    // If the view has changed, the task will yield a new view message.
    // Then the oneshot sender is used to tell the caller that the view
    // is now up to date (even if no new value was yielded).
    force_update_view_sender: mpsc::Sender<oneshot::Sender<()>>,
    view_update_task: tokio::task::JoinHandle<()>,
}

impl Drop for Wallet {
    fn drop(&mut self) {
        // TODO: We should properly shut down the task rather than aborting it.
        self.view_update_task.abort();
    }
}

impl Wallet {
    pub async fn new(fedimint_clients_data_dir: PathBuf, network: Network) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&fedimint_clients_data_dir).await?;

        let (view_update_sender, view_update_receiver) = watch::channel(WalletView {
            federations: BTreeMap::new(),
        });

        let (force_update_view_sender, mut force_update_view_receiver) =
            mpsc::channel::<oneshot::Sender<()>>(100);

        let clients = Arc::new(RwLock::new(HashMap::new()));

        let clients_clone = clients.clone();
        let view_update_task = tokio::spawn(async move {
            let mut last_state_or = None;

            // TODO: Optimize this. Repeated polling is not ideal.
            loop {
                // Wait either for a force update or for a timeout. If a force update
                // occurs, then `force_update_completed_oneshot_or` will be `Some`.
                // If a timeout occurs, then `force_update_completed_oneshot_or` will be `None`.
                // TODO: Investigate why `tokio::select!` causes this clippy lint to fire.
                #[allow(clippy::redundant_pub_crate)]
                let force_update_completed_oneshot_or = tokio::select! {
                    Some(force_update_completed_oneshot) = force_update_view_receiver.recv() => Some(force_update_completed_oneshot),
                    () = tokio::time::sleep(WALLET_VIEW_UPDATE_INTERVAL) => None,
                };

                match Self::get_current_state(&clients_clone.read().await).await {
                    Ok(current_state) => {
                        // Ignoring clippy lint here since the `match` provides better clarity.
                        #[allow(clippy::option_if_let_else)]
                        let has_changed = match &last_state_or {
                            Some(last_state) => &current_state != last_state,
                            // If there was no last state, the state has changed.
                            None => true,
                        };

                        if has_changed {
                            last_state_or = Some(current_state.clone());

                            // If all receivers have been dropped, stop the task.
                            if view_update_sender.send(current_state).is_err() {
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "Failed to update Vendimint wallet view");
                    }
                }

                // If this iteration was triggered by a force update, then send a message
                // back to the caller to indicate that the view is now up to date.
                if let Some(force_update_completed_oneshot) = force_update_completed_oneshot_or {
                    let _ = force_update_completed_oneshot.send(());
                }
            }
        });

        let mnemonic_path = fedimint_clients_data_dir.join(MNEMONIC_PATH);

        if !tokio::fs::try_exists(&mnemonic_path).await? {
            let mnemonic = bip39::Mnemonic::generate(12).expect("12-word mnemonics are valid");
            tokio::fs::write(&mnemonic_path, mnemonic.to_string()).await?;
        }

        let mnemonic_string = tokio::fs::read_to_string(&mnemonic_path).await?;
        let mnemonic = bip39::Mnemonic::from_str(&mnemonic_string).expect("Valid mnemonic");

        let xprivkey = Xpriv::new_master(network, &mnemonic.to_seed_normalized(MNEMONIC_PASSWORD))
            .expect("Can never fail (see `new_master`'s implementation)");

        let wallet = Self {
            root_secret: get_root_secret(&xprivkey),
            clients,
            fedimint_clients_data_dir: Mutex::from(fedimint_clients_data_dir),
            view_update_receiver,
            force_update_view_sender,
            view_update_task,
        };

        wallet.connect_to_joined_federations().await?;

        Ok(wallet)
    }

    // TODO: Use this method or remove it.
    #[allow(dead_code)]
    pub fn get_update_stream(&self) -> tokio_stream::wrappers::WatchStream<WalletView> {
        tokio_stream::wrappers::WatchStream::new(self.view_update_receiver.clone())
    }

    /// Tells `view_update_task` to update the view, and waits for it to complete.
    /// This ensures any streams opened by `get_update_stream` have yielded the
    /// latest view. This function should be called at the end of any function
    /// that modifies the view.
    async fn force_update_view(&self, clients: RwLockWriteGuard<'_, FederationClients>) {
        // While this function doesn't need to take the `clients` argument, it
        // does so to make it clear that any calling function must not hold a
        // write lock when calling this function. This is to prevent deadlocks,
        // since the task that responds to the channel here requires a read lock
        // on the clients map.
        drop(clients);
        let (sender, receiver) = oneshot::channel();
        let _ = self.force_update_view_sender.send(sender).await;
        let _ = receiver.await;
    }

    pub async fn get_lnv2_claim_pubkey(&self, federation_id: FederationId) -> Option<PublicKey> {
        let clients = self.clients.read().await;

        let federation = clients.get(&federation_id)?;

        let lightning_module = federation
            .client
            .get_first_module::<LightningClientModule>()
            .ok()?;

        Some(lightning_module.get_public_key())
    }

    pub async fn get_mint_version(&self, federation_id: FederationId) -> Option<MintVersion> {
        self.clients
            .read()
            .await
            .get(&federation_id)
            .map(|federation| federation.mint_version)
    }

    async fn connect_to_joined_federations(&self) -> anyhow::Result<()> {
        let fedimint_clients_data_dir = self.fedimint_clients_data_dir.lock().await;

        // List all files in the data directory.
        let mut read_dir = tokio::fs::read_dir(fedimint_clients_data_dir.as_path()).await?;
        let mut federation_ids = Vec::<FederationId>::new();
        while let Some(entry) = read_dir.next_entry().await? {
            if let Ok(federation_id_str) = entry.file_name().into_string()
                && let Ok(federation_id) = federation_id_str.parse()
            {
                federation_ids.push(federation_id);
            }
        }

        let mut clients = self.clients.write().await;

        for federation_id in federation_ids {
            // Skip if we're already connected to this federation.
            if clients.contains_key(&federation_id) {
                continue;
            }

            let db: Database =
                RocksDb::build(fedimint_clients_data_dir.join(federation_id.to_string()))
                    .open()
                    .await?
                    .into();

            let client = self
                .build_client_from_federation_id(
                    fedimint_clients_data_dir.as_path(),
                    federation_id,
                    db,
                )
                .await?;

            clients.insert(federation_id, client);
        }

        self.force_update_view(clients).await;

        Ok(())
    }

    pub async fn set_default_federation(&self, invite_code: InviteCode) -> anyhow::Result<()> {
        let federation_id = invite_code.federation_id();

        let fedimint_clients_data_dir = self.fedimint_clients_data_dir.lock().await;

        let federation_data_dir = fedimint_clients_data_dir.join(federation_id.to_string());

        // Short-circuit if we're already connected to this federation. Checking
        // the live client map rather than the directory also permits retrying a
        // join that previously failed after creating its RocksDB directory.
        if !self.clients.read().await.contains_key(&federation_id) {
            let db: Database = RocksDb::build(&federation_data_dir).open().await?.into();

            let client = self
                .build_client_from_invite_code(
                    fedimint_clients_data_dir.as_path(),
                    invite_code.clone(),
                    db,
                )
                .await?;

            let mut clients = self.clients.write().await;
            clients.insert(federation_id, client);
            self.force_update_view(clients).await;
        }

        // Record the default federation invite code on disk
        tokio::fs::write(
            fedimint_clients_data_dir.join(DEFAULT_FEDERATION_PATH),
            invite_code.to_string(),
        )
        .await?;

        Ok(())
    }

    pub async fn get_default_federation(&self) -> std::io::Result<Option<InviteCode>> {
        let fedimint_clients_data_dir = self.fedimint_clients_data_dir.lock().await;
        let default_path = fedimint_clients_data_dir.join(DEFAULT_FEDERATION_PATH);

        if !default_path.exists() {
            return Ok(None);
        }

        let invite_str = tokio::fs::read_to_string(default_path).await?;
        match invite_str.trim().parse() {
            Ok(invite) => Ok(Some(invite)),
            Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        }
    }

    // TODO: Call `ClientModule::leave()` for every module.
    // https://docs.rs/fedimint-client/0.4.2/fedimint_client/module/trait.ClientModule.html#method.leave
    // Currently it isn't implemented for the `LightningClientModule`, so for now we're just checking
    // that the client has a zero balance.
    // TODO: Use this method or remove it.
    #[allow(dead_code)]
    pub async fn leave_federation(&self, federation_id: FederationId) -> anyhow::Result<()> {
        let mut clients = self.clients.write().await;

        if let Some(federation) = clients.remove(&federation_id) {
            let balance = match federation.client.get_balance_for_btc().await {
                Ok(balance) => balance,
                Err(error) => {
                    clients.insert(federation_id, federation);
                    return Err(error.context("could not read the balance before leaving"));
                }
            };
            if balance.msats != 0 {
                // Re-insert the client back into the clients map.
                clients.insert(federation_id, federation);

                return Err(anyhow!(
                    "Cannot leave federation with non-zero balance: {federation_id}"
                ));
            }

            federation.client.shutdown().await;

            let fedimint_clients_data_dir = self.fedimint_clients_data_dir.lock().await;

            let federation_data_dir = fedimint_clients_data_dir.join(federation_id.to_string());

            if federation_data_dir.is_dir() {
                tokio::fs::remove_dir_all(federation_data_dir).await?;
            }

            let mint_selection_path =
                Self::mint_selection_path(fedimint_clients_data_dir.as_path(), federation_id);
            match tokio::fs::remove_file(mint_selection_path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }

        self.force_update_view(clients).await;

        Ok(())
    }

    /// Constructs the current view of the wallet.
    /// SHOULD ONLY BE CALLED FROM THE `view_update_task`.
    /// This way, `view_update_task` can only yield values
    /// when the view is changed, with the guarantee that
    /// the view hasn't been updated elsewhere in a way that
    /// could de-sync the view.
    async fn get_current_state(
        clients: &RwLockReadGuard<'_, FederationClients>,
    ) -> anyhow::Result<WalletView> {
        let mut federations = BTreeMap::new();

        for (federation_id, federation) in clients.iter() {
            federations.insert(
                *federation_id,
                FederationView {
                    federation_id: *federation_id,
                    name: federation
                        .client
                        .config()
                        .await
                        .global
                        .federation_name()
                        .map(ToString::to_string),
                    balance: federation
                        .client
                        .get_balance_for_btc()
                        .await
                        .with_context(|| {
                            format!("could not read balance for federation {federation_id}")
                        })?,
                    mint_version: federation.mint_version,
                },
            );
        }

        Ok(WalletView { federations })
    }

    // TODO: Return a strongly typed result.
    pub async fn receive_payment(
        &self,
        federation_id: FederationId,
        claimer_pk: PublicKey,
        amount: Amount,
        expiry_secs: u32,
        description: Bolt11InvoiceDescription,
        gateway: Option<SafeUrl>,
    ) -> anyhow::Result<(Bolt11Invoice, OperationId)> {
        let clients = self.clients.read().await;

        let federation = clients
            .get(&federation_id)
            .ok_or_else(|| anyhow!("Client for federation {federation_id} not found"))?;

        let lightning_module = federation
            .client
            .get_first_module::<LightningClientModule>()
            .context("the federation does not have a compatible lnv2 module")?;

        Ok(lightning_module
            .remote_receive(claimer_pk, amount, expiry_secs, description, gateway)
            .await?)
    }

    pub async fn await_receive_payment_final_state(
        &self,
        operation_id: OperationId,
    ) -> anyhow::Result<FinalRemoteReceiveOperationState> {
        let clients = self.clients.read().await;

        for federation in clients.values() {
            if federation.client.operation_exists(operation_id).await {
                let lightning_module = federation
                    .client
                    .get_first_module::<LightningClientModule>()
                    .context("the federation does not have a compatible lnv2 module")?;

                return lightning_module.await_remote_receive(operation_id).await;
            }
        }

        Err(anyhow!(
            "Client not found containing operation id {}",
            operation_id.0.to_upper_hex_string()
        ))
    }

    pub async fn get_local_balance(&self) -> anyhow::Result<Amount> {
        let mut balance = Amount::ZERO;

        let clients = self.clients.read().await;

        for (federation_id, federation) in clients.iter() {
            balance += federation
                .client
                .get_balance_for_btc()
                .await
                .with_context(|| {
                    format!("could not read balance for federation {federation_id}")
                })?;
        }
        Ok(balance)
    }

    pub async fn sweep_all_ecash_notes<M: Serialize + Send>(
        &self,
        federation_id: FederationId,
        try_cancel_after: Duration,
        include_invite: bool,
        extra_meta: M,
    ) -> anyhow::Result<Option<EcashExport>> {
        let clients = self.clients.read().await;

        let federation = clients
            .get(&federation_id)
            .ok_or_else(|| anyhow!("Client for federation {federation_id} not found"))?;

        match federation.mint_version {
            MintVersion::V1 => {
                let mint_module = federation
                    .client
                    .get_first_module::<MintV1ClientModule>()
                    .context("the selected mint v1 module is unavailable")?;

                let ecash_balance = mint_module
                    .get_note_counts_by_denomination(
                        &mut mint_module.db.begin_transaction().await.to_ref_nc(),
                    )
                    .await
                    .total_amount();

                // This is needed because `spend_notes_with_selector`
                // will panic if the requested amount is zero.
                if ecash_balance == Amount::ZERO {
                    return Ok(None);
                }

                // Note: Since we use the `SelectNotesWithExactAmount` note selector, we
                // could hit a race condition if this method is called twice concurrently.
                // Both calls could get the same value for `ecash_balance`, but then one
                // of the calls sweeps all of the notes while the other call gets an empty
                // set of notes, resulting in that call's `spend_notes_with_selector`
                // failing with an error. In practice this shouldn't matter much, but it'd
                // be better to eventually use a more tailor-built note selector.
                let (_operation_id, oob_notes) = mint_module
                    .spend_notes_with_selector(
                        &SelectNotesWithExactAmount,
                        ecash_balance,
                        try_cancel_after,
                        include_invite,
                        extra_meta,
                    )
                    .await?;

                Ok(Some(EcashExport {
                    token: oob_notes.to_string(),
                    amount: oob_notes.total_amount(),
                    mint_version: MintVersion::V1,
                    reclaims_automatically: true,
                }))
            }
            MintVersion::V2 => {
                let mint_module = federation
                    .client
                    .get_first_module::<MintV2ClientModule>()
                    .context("the selected mint v2 module is unavailable")?;
                let ecash_balance = federation
                    .client
                    .get_balance_for_btc()
                    .await
                    .context("could not read the mint v2 balance")?;
                if ecash_balance == Amount::ZERO {
                    return Ok(None);
                }

                let ecash = mint_module
                    .send(ecash_balance, serde_json::to_value(extra_meta)?)
                    .await?;

                Ok(Some(EcashExport {
                    token: encode_prefixed(FEDIMINT_PREFIX, &ecash),
                    amount: ecash.amount(),
                    mint_version: MintVersion::V2,
                    reclaims_automatically: false,
                }))
            }
        }
    }

    /// Get claimable contracts from a federation.
    ///
    /// Returns `Some` if we're connected to the federation, otherwise `None`.
    pub async fn get_claimable_contracts(
        &self,
        federation_id: FederationId,
        claimer_pk: PublicKey,
        limit_or: Option<usize>,
    ) -> Option<Vec<ClaimableContract>> {
        let clients = self.clients.read().await;

        let federation = clients.get(&federation_id)?;

        let lightning_module = federation
            .client
            .get_first_module::<LightningClientModule>()
            .ok()?;

        let contracts = lightning_module
            .get_claimable_contracts(claimer_pk, limit_or)
            .await;

        Some(contracts)
    }

    pub async fn remove_claimed_contracts(
        &self,
        federation_id: FederationId,
        contract_ids: Vec<ContractId>,
    ) {
        let clients = self.clients.read().await;

        let Some(federation) = clients.get(&federation_id) else {
            return;
        };

        let Ok(lightning_module) = federation
            .client
            .get_first_module::<LightningClientModule>()
        else {
            return;
        };

        lightning_module
            .remove_claimed_contracts(contract_ids)
            .await;
    }

    pub async fn claim_contracts(
        &self,
        federation_id: FederationId,
        claimable_contracts: Vec<ClaimableContract>,
    ) -> anyhow::Result<()> {
        let clients = self.clients.read().await;

        let Some(federation) = clients.get(&federation_id) else {
            return Err(anyhow!("Client not found"));
        };

        let lightning_module = federation
            .client
            .get_first_module::<LightningClientModule>()
            .context("the federation does not have a compatible lnv2 module")?;

        lightning_module.claim_contracts(claimable_contracts).await
    }

    async fn build_client_from_invite_code(
        &self,
        data_dir: &Path,
        invite_code: InviteCode,
        db: Database,
    ) -> anyhow::Result<FederationClient> {
        let is_initialized = fedimint_client::Client::is_initialized(&db).await;
        if is_initialized {
            return self
                .build_client_from_federation_id(data_dir, invite_code.federation_id(), db)
                .await;
        }

        // Preview first so Vendimint can explicitly select one e-cash module.
        // Registering both would leave the primary-module choice to instance-id
        // ordering because mint v1 and v2 currently advertise equal priority.
        let config = Client::builder()
            .await?
            .preview(Self::build_client_connectors().await?, &invite_code)
            .await?
            .config()
            .clone();
        let mint_version = Self::preferred_mint_version(&config)?;
        Self::persist_mint_version(data_dir, invite_code.federation_id(), mint_version).await?;

        let client = Self::client_builder(mint_version)
            .await?
            .preview_with_existing_config(
                Self::build_client_connectors().await?,
                config,
                invite_code.api_secret(),
            )
            .await?
            .join(db, self.root_secret.clone())
            .await?;

        Ok(FederationClient {
            client,
            mint_version,
        })
    }

    async fn build_client_from_federation_id(
        &self,
        data_dir: &Path,
        federation_id: FederationId,
        db: Database,
    ) -> anyhow::Result<FederationClient> {
        let is_initialized = fedimint_client::Client::is_initialized(&db).await;
        if !is_initialized {
            bail!("Federation with ID {federation_id} is not initialized.");
        }

        let probe = Client::builder().await?;
        let config = probe.load_existing_config(&db).await?;
        let mint_version =
            if let Some(mint_version) = Self::load_mint_version(data_dir, federation_id).await? {
                Self::validate_mint_version(&config, mint_version)?;
                mint_version
            } else {
                let mint_version = Self::legacy_mint_version(&config)?;
                Self::persist_mint_version(data_dir, federation_id, mint_version).await?;
                mint_version
            };

        let client = Self::client_builder(mint_version)
            .await?
            .open(
                Self::build_client_connectors().await?,
                db,
                self.root_secret.clone(),
            )
            .await?;

        Ok(FederationClient {
            client,
            mint_version,
        })
    }

    async fn client_builder(mint_version: MintVersion) -> anyhow::Result<ClientBuilder> {
        let mut builder = Client::builder().await?;
        match mint_version {
            MintVersion::V1 => builder.with_module(MintV1ClientInit),
            MintVersion::V2 => builder.with_module(MintV2ClientInit),
        }
        builder.with_module(LightningRemoteClientInit::default());
        Ok(builder)
    }

    fn preferred_mint_version(config: &ClientConfig) -> anyhow::Result<MintVersion> {
        Self::preferred_mint_version_from_support(
            Self::has_module(config, LNV2_KIND),
            Self::has_module(config, MINT_V1_KIND),
            Self::has_module(config, MINT_V2_KIND),
        )
    }

    fn preferred_mint_version_from_support(
        has_lnv2: bool,
        has_v1: bool,
        has_v2: bool,
    ) -> anyhow::Result<MintVersion> {
        if !has_lnv2 {
            bail!("Federation does not have a compatible lnv2 module");
        }
        if has_v2 {
            Ok(MintVersion::V2)
        } else if has_v1 {
            Ok(MintVersion::V1)
        } else {
            bail!("Federation has neither a compatible mint nor mintv2 module")
        }
    }

    fn legacy_mint_version(config: &ClientConfig) -> anyhow::Result<MintVersion> {
        Self::legacy_mint_version_from_support(
            Self::has_module(config, LNV2_KIND),
            Self::has_module(config, MINT_V1_KIND),
            Self::has_module(config, MINT_V2_KIND),
        )
    }

    fn legacy_mint_version_from_support(
        has_lnv2: bool,
        has_v1: bool,
        has_v2: bool,
    ) -> anyhow::Result<MintVersion> {
        if !has_lnv2 {
            bail!("Federation does not have a compatible lnv2 module");
        }
        match (has_v1, has_v2) {
            // Before Vendimint persisted a selection, it only registered mint
            // v1. A marker-less initialized wallet therefore used v1 even if
            // its federation config also advertised mint v2.
            (true, _) => Ok(MintVersion::V1),
            (false, true) => Ok(MintVersion::V2),
            (false, false) => {
                bail!("Federation has neither a compatible mint nor mintv2 module")
            }
        }
    }

    fn validate_mint_version(
        config: &ClientConfig,
        mint_version: MintVersion,
    ) -> anyhow::Result<()> {
        Self::require_lnv2(config)?;
        if !Self::has_module(config, mint_version.module_kind()) {
            bail!(
                "Federation no longer advertises the pinned {} module",
                mint_version.module_kind()
            );
        }
        Ok(())
    }

    fn require_lnv2(config: &ClientConfig) -> anyhow::Result<()> {
        if !Self::has_module(config, LNV2_KIND) {
            bail!("Federation does not have a compatible lnv2 module");
        }
        Ok(())
    }

    fn has_module(config: &ClientConfig, kind: &str) -> bool {
        config
            .modules
            .values()
            .any(|module| module.kind().as_str() == kind)
    }

    fn mint_selection_path(data_dir: &Path, federation_id: FederationId) -> PathBuf {
        data_dir.join(format!("{federation_id}{MINT_SELECTION_SUFFIX}"))
    }

    async fn load_mint_version(
        data_dir: &Path,
        federation_id: FederationId,
    ) -> anyhow::Result<Option<MintVersion>> {
        let path = Self::mint_selection_path(data_dir, federation_id);
        let encoded = match tokio::fs::read(&path).await {
            Ok(encoded) => encoded,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        serde_json::from_slice(&encoded)
            .with_context(|| format!("could not decode mint selection at {}", path.display()))
            .map(Some)
    }

    async fn persist_mint_version(
        data_dir: &Path,
        federation_id: FederationId,
        mint_version: MintVersion,
    ) -> anyhow::Result<()> {
        let path = Self::mint_selection_path(data_dir, federation_id);
        if let Some(existing) = Self::load_mint_version(data_dir, federation_id).await? {
            if existing != mint_version {
                bail!("Federation is pinned to {existing}, refusing to switch to {mint_version}");
            }
            return Ok(());
        }

        let temporary_path = path.with_file_name(format!(
            "{}.tmp",
            path.file_name()
                .expect("mint selection path always has a file name")
                .to_string_lossy()
        ));
        tokio::fs::write(&temporary_path, serde_json::to_vec(&mint_version)?).await?;
        tokio::fs::File::open(&temporary_path)
            .await?
            .sync_all()
            .await?;
        tokio::fs::rename(&temporary_path, &path).await?;
        Ok(())
    }

    async fn build_client_connectors() -> anyhow::Result<ConnectorRegistry> {
        ConnectorRegistry::build_from_client_env()?.bind().await
    }
}

fn get_root_secret(xprivkey: &Xpriv) -> RootSecret {
    let context = Secp256k1::new();

    let xpriv = xprivkey
        .derive_priv(
            &context,
            &[
                ChildNumber::from_hardened_idx(coin_type_from_network(xprivkey.network))
                    .expect("Should only fail if 2^31 <= index"),
            ],
        )
        .expect("This can never fail. Should be fixed in future version of `bitcoin` crate.");

    // `Mnemonic::from_entropy()` should only ever fail if the input is not of the correct length.
    // Valid lengths are 128, 160, 192, 224, or 256 bits, and `SecretKey::secret_bytes()` is always 256 bits.
    let mnemonic = Mnemonic::from_entropy(&xpriv.private_key.secret_bytes())
        .expect("Private key should always be 32 bytes");

    RootSecret::StandardDoubleDerive(Bip39RootSecretStrategy::<12>::to_root_secret(&mnemonic))
}

const fn coin_type_from_network(network_kind: NetworkKind) -> u32 {
    match network_kind {
        NetworkKind::Main => 0,
        NetworkKind::Test => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_federations_prefer_mint_v2() {
        assert_eq!(
            Wallet::preferred_mint_version_from_support(true, true, true).unwrap(),
            MintVersion::V2
        );
        assert_eq!(
            Wallet::preferred_mint_version_from_support(true, true, false).unwrap(),
            MintVersion::V1
        );
        assert_eq!(
            Wallet::preferred_mint_version_from_support(true, false, true).unwrap(),
            MintVersion::V2
        );
    }

    #[test]
    fn incompatible_federations_are_rejected() {
        let missing_lnv2 =
            Wallet::preferred_mint_version_from_support(false, true, true).unwrap_err();
        assert!(missing_lnv2.to_string().contains("lnv2"));

        let missing_mint =
            Wallet::preferred_mint_version_from_support(true, false, false).unwrap_err();
        assert!(missing_mint.to_string().contains("neither"));
    }

    #[test]
    fn legacy_wallets_remain_on_mint_v1() {
        assert_eq!(
            Wallet::legacy_mint_version_from_support(true, true, true).unwrap(),
            MintVersion::V1
        );
        assert_eq!(
            Wallet::legacy_mint_version_from_support(true, true, false).unwrap(),
            MintVersion::V1
        );
    }

    #[test]
    fn mint_selection_encoding_is_stable() {
        assert_eq!(serde_json::to_string(&MintVersion::V1).unwrap(), "\"mint\"");
        assert_eq!(
            serde_json::to_string(&MintVersion::V2).unwrap(),
            "\"mintv2\""
        );
    }
}
