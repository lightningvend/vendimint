use std::{
    collections::{BTreeMap, HashMap},
    fmt::Display,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use bip39::Mnemonic;
use bitcoin::{
    Network, NetworkKind,
    bip32::{ChildNumber, Xpriv},
    hex::DisplayHex,
    secp256k1::{PublicKey, Secp256k1},
};
use fedimint_bip39::Bip39RootSecretStrategy;
use fedimint_client::{
    Client, ClientHandle, ModuleKind, OperationId, RootSecret, secret::RootSecretStrategy,
};
use fedimint_core::{
    Amount, config::FederationId, db::Database, invite_code::InviteCode, util::SafeUrl,
};
use fedimint_lnv2_common::{Bolt11InvoiceDescription, ContractId};
use fedimint_lnv2_remote_client::{
    ClaimableContract, FinalRemoteReceiveOperationState, LightningClientModule,
    LightningRemoteClientInit,
};
use fedimint_mint_client::{
    MintClientInit, MintClientModule, OOBNotes, SelectNotesWithExactAmount,
};
use fedimint_rocksdb::RocksDb;
use lightning_invoice::Bolt11Invoice;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, mpsc, oneshot, watch};

use crate::machine::ReceivePaymentError;

const WALLET_VIEW_UPDATE_INTERVAL: Duration = Duration::from_secs(5);

const MNEMONIC_PATH: &str = "mnemonic.txt";
const MNEMONIC_PASSWORD: &str = "";
const DEFAULT_FEDERATION_PATH: &str = "default_federation.txt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletView {
    pub federations: BTreeMap<FederationId, FederationView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationView {
    pub federation_id: FederationId,
    pub name: Option<String>,
    pub balance: Amount,
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
    clients: Arc<RwLock<HashMap<FederationId, ClientHandle>>>,
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

                let current_state = Self::get_current_state(&clients_clone.read().await).await;

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
    async fn force_update_view(
        &self,
        clients: RwLockWriteGuard<'_, HashMap<FederationId, ClientHandle>>,
    ) {
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

        let client = clients.get(&federation_id)?;

        let lightning_module = client.get_first_module::<LightningClientModule>().unwrap();

        Some(lightning_module.get_public_key())
    }

    async fn connect_to_joined_federations(&self) -> anyhow::Result<()> {
        let fedimint_clients_data_dir = self.fedimint_clients_data_dir.lock().await;

        // List all files in the data directory.
        let mut read_dir = tokio::fs::read_dir(fedimint_clients_data_dir.as_path()).await?;
        let mut federation_ids = Vec::<FederationId>::new();
        while let Some(entry) = read_dir.next_entry().await? {
            if let Ok(federation_id_str) = entry.file_name().into_string() {
                if let Ok(federation_id) = federation_id_str.parse() {
                    federation_ids.push(federation_id);
                }
            }
        }

        let mut clients = self.clients.write().await;

        for federation_id in federation_ids {
            // Skip if we're already connected to this federation.
            if clients.contains_key(&federation_id) {
                continue;
            }

            let db: Database =
                RocksDb::open(fedimint_clients_data_dir.join(federation_id.to_string()))
                    .await?
                    .into();

            let client = self
                .build_client_from_federation_id(federation_id, db)
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

        // Short-circuit if we're already connected to this federation.
        if !federation_data_dir.is_dir() {
            let db: Database = RocksDb::open(&federation_data_dir).await?.into();

            let client = self
                .build_client_from_invite_code(invite_code.clone(), db)
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

        if let Some(client) = clients.remove(&federation_id) {
            if client.get_balance().await.msats != 0 {
                // Re-insert the client back into the clients map.
                clients.insert(federation_id, client);

                return Err(anyhow::anyhow!(
                    "Cannot leave federation with non-zero balance: {federation_id}"
                ));
            }

            client.shutdown().await;

            let fedimint_clients_data_dir = self.fedimint_clients_data_dir.lock().await;

            let federation_data_dir = fedimint_clients_data_dir.join(federation_id.to_string());

            if federation_data_dir.is_dir() {
                tokio::fs::remove_dir_all(federation_data_dir).await?;
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
        clients: &RwLockReadGuard<'_, HashMap<FederationId, ClientHandle>>,
    ) -> WalletView {
        let mut federations = BTreeMap::new();

        for (federation_id, client) in clients.iter() {
            federations.insert(
                *federation_id,
                FederationView {
                    federation_id: *federation_id,
                    name: client
                        .config()
                        .await
                        .global
                        .federation_name()
                        .map(ToString::to_string),
                    balance: client.get_balance().await,
                },
            );
        }

        WalletView { federations }
    }

    pub async fn receive_payment(
        &self,
        federation_id: FederationId,
        claimer_pk: PublicKey,
        amount: Amount,
        expiry_secs: u32,
        description: Bolt11InvoiceDescription,
        gateway: Option<SafeUrl>,
    ) -> Result<(Bolt11Invoice, OperationId), ReceivePaymentError> {
        let clients = self.clients.read().await;

        let client = clients
            .get(&federation_id)
            .ok_or_else(|| ReceivePaymentError::MachineNotConnectedToFederation)?;

        let lightning_module = client.get_first_module::<LightningClientModule>().unwrap();

        lightning_module
            .remote_receive(claimer_pk, amount, expiry_secs, description, gateway)
            .await
            .map_err(ReceivePaymentError::RemoteReceiveError)
    }

    pub async fn await_receive_payment(
        &self,
        operation_id: OperationId,
    ) -> anyhow::Result<FinalRemoteReceiveOperationState> {
        let clients = self.clients.read().await;

        for client in clients.values() {
            if client.operation_exists(operation_id).await {
                let lightning_module = client.get_first_module::<LightningClientModule>().unwrap();

                return lightning_module.await_remote_receive(operation_id).await;
            }
        }

        Err(anyhow::anyhow!(
            "Client not found containing operation id {}",
            operation_id.0.to_upper_hex_string()
        ))
    }

    pub async fn get_local_balance(&self) -> Amount {
        let mut balance = Amount::ZERO;

        let clients = self.clients.read().await;

        for client in clients.values() {
            balance += client.get_balance().await;
        }
        balance
    }

    pub async fn sweep_all_ecash_notes<M: Serialize + Send>(
        &self,
        federation_id: FederationId,
        try_cancel_after: Duration,
        include_invite: bool,
        extra_meta: M,
    ) -> anyhow::Result<Option<OOBNotes>> {
        let clients = self.clients.read().await;

        let client = clients
            .get(&federation_id)
            .ok_or_else(|| anyhow::anyhow!("Client for federation {federation_id} not found"))?;

        let mint_module = client.get_first_module::<MintClientModule>().unwrap();

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

        Ok(Some(oob_notes))
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

        let client = clients.get(&federation_id)?;

        let lightning_module = client.get_first_module::<LightningClientModule>().unwrap();

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

        let Some(client) = clients.get(&federation_id) else {
            return;
        };

        let lightning_module = client.get_first_module::<LightningClientModule>().unwrap();

        lightning_module
            .remove_claimed_contracts(contract_ids)
            .await;
    }

    pub async fn claim_contract(
        &self,
        federation_id: FederationId,
        claimable_contract: ClaimableContract,
    ) -> anyhow::Result<()> {
        let clients = self.clients.read().await;

        let Some(client) = clients.get(&federation_id) else {
            return Err(anyhow::anyhow!("Client not found"));
        };

        let lightning_module = client.get_first_module::<LightningClientModule>().unwrap();

        lightning_module.claim_contract(claimable_contract).await
    }

    async fn build_client_from_invite_code(
        &self,
        invite_code: InviteCode,
        db: Database,
    ) -> anyhow::Result<ClientHandle> {
        let is_initialized = fedimint_client::Client::is_initialized(&db).await;

        let mut client_builder = Client::builder(db).await?;

        // Add lightning and e-cash modules. For now we don't support on-chain.
        client_builder.with_module(MintClientInit);
        client_builder.with_module(LightningRemoteClientInit::default());

        client_builder.with_primary_module_kind(ModuleKind::from_static_str("mint"));

        let root_secret = self.root_secret.clone();

        let client = if is_initialized {
            client_builder.open(root_secret).await?
        } else {
            client_builder
                .preview(&invite_code)
                .await?
                .join(root_secret)
                .await?
        };

        Ok(client)
    }

    async fn build_client_from_federation_id(
        &self,
        federation_id: FederationId,
        db: Database,
    ) -> anyhow::Result<ClientHandle> {
        let is_initialized = fedimint_client::Client::is_initialized(&db).await;

        let mut client_builder = Client::builder(db).await?;

        // Add lightning and e-cash modules. For now we don't support on-chain.
        client_builder.with_module(MintClientInit);
        client_builder.with_module(LightningRemoteClientInit::default());

        client_builder.with_primary_module_kind(ModuleKind::from_static_str("mint"));

        let root_secret = self.root_secret.clone();

        let client = if is_initialized {
            client_builder.open(root_secret).await?
        } else {
            return Err(anyhow::anyhow!(
                "Federation with ID {federation_id} is not initialized."
            ));
        };

        Ok(client)
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
