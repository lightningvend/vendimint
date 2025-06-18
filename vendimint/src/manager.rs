use std::{path::Path, sync::Arc, time::Duration};

use crate::fedimint_wallet::Wallet;
use crate::vendimint_iroh::MachineConfig;
use crate::vendimint_iroh::ManagerProtocol;
use bitcoin::Network;
use bitcoin::secp256k1::PublicKey;
use fedimint_core::Amount;
use fedimint_core::{config::FederationId, invite_code::InviteCode};
use fedimint_mint_client::OOBNotes;
use iroh::NodeAddr;
use serde::Serialize;
use tokio::sync::oneshot;
use uuid::Uuid;

const PROTOCOL_SUBDIR: &str = "protocol";
const FEDIMINT_SUBDIR: &str = "fedimint";

pub struct Manager {
    iroh_protocol: Arc<ManagerProtocol>,
    wallet: Arc<Wallet>,

    /// Task which moves claimable payments from `Self.iroh_protocol` to `Self.wallet`.
    /// This is safe to cancel/abort at any time, since it copies payments over to the new
    /// location before deleting them from the old location, and all write operations are
    /// idempotent. Is `None` after `Self::shutdown` has been called, otherwise is `Some`.
    syncer_task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Manager {
    pub async fn new(storage_path: &Path, network: Network) -> anyhow::Result<Self> {
        let network_partitioned_storage_path = storage_path.join(network.to_string());

        let iroh_protocol = Arc::new(
            ManagerProtocol::new(&network_partitioned_storage_path.join(PROTOCOL_SUBDIR)).await?,
        );

        let wallet = Arc::new(Wallet::new(
            network_partitioned_storage_path.join(FEDIMINT_SUBDIR),
            network,
        )?);

        wallet.connect_to_joined_federations().await?;

        let iroh_protocol_clone = iroh_protocol.clone();
        let wallet_clone = wallet.clone();
        let syncer_task_handle = tokio::task::spawn(async move {
            loop {
                Self::sweep_payments(&iroh_protocol_clone, &wallet_clone).await;

                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });

        Ok(Self {
            iroh_protocol,
            wallet,
            syncer_task_handle: Some(syncer_task_handle),
        })
    }

    /// Shuts down the manager idempotently.
    ///
    /// After awaiting this, [`Self::is_shutdown`] will return `true`.
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.iroh_protocol.shutdown().await?;

        // This task is safe to abort. See the property's documentation for why this is the case.
        if let Some(syncer_task_handle) = self.syncer_task_handle.take() {
            syncer_task_handle.abort();
            let _ = syncer_task_handle.await;
        }

        Ok(())
    }

    /// Checks if the manager is already shutdown.
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.iroh_protocol.is_shutdown() && self.syncer_task_handle.is_none()
    }

    pub async fn claim_machine(
        &self,
        node_addr: NodeAddr,
    ) -> anyhow::Result<(u32, oneshot::Sender<bool>)> {
        self.iroh_protocol.claim_machine(node_addr).await
    }

    pub fn list_machine_ids(&self) -> anyhow::Result<Vec<Uuid>> {
        Ok(self
            .iroh_protocol
            .list_machines()?
            .into_iter()
            .map(|(machine_id, _)| machine_id)
            .collect())
    }

    pub async fn get_machine_config(
        &self,
        machine_id: &Uuid,
    ) -> anyhow::Result<Option<MachineConfig>> {
        self.iroh_protocol.get_machine_config(machine_id).await
    }

    pub async fn set_machine_config(
        &self,
        machine_id: &Uuid,
        machine_config: &MachineConfig,
    ) -> anyhow::Result<()> {
        self.iroh_protocol
            .set_machine_config(machine_id, machine_config)
            .await
    }

    pub async fn join_federation(&self, invite_code: InviteCode) -> anyhow::Result<()> {
        self.wallet.join_federation(invite_code).await
    }

    pub async fn get_fedimint_lnv2_claim_pubkey(
        &self,
        federation_id: FederationId,
    ) -> Option<PublicKey> {
        self.wallet.get_lnv2_claim_pubkey(federation_id).await
    }

    pub async fn get_local_balance(&self) -> Amount {
        self.wallet.get_local_balance().await
    }

    pub async fn sweep_all_ecash_notes<M: Serialize + Send>(
        &self,
        federation_id: FederationId,
        try_cancel_after: Duration,
        include_invite: bool,
        extra_meta: M,
    ) -> anyhow::Result<Option<OOBNotes>> {
        self.wallet
            .sweep_all_ecash_notes(federation_id, try_cancel_after, include_invite, extra_meta)
            .await
    }

    /// Make completed payments syncable to the manager.
    /// This must be called periodically to ensure that
    /// the manager is able to sweep completed payments.
    async fn sweep_payments(manager_protocol: &Arc<ManagerProtocol>, wallet: &Arc<Wallet>) {
        // 1. Fetch claimable contracts from the iroh doc that we can sweep.
        let Ok(claimable_contracts) = manager_protocol.get_claimable_contracts().await else {
            return;
        };

        // 2. Sweep the payments to the wallet.
        let mut claimed_contracts = Vec::new();
        for (machine_id, federation_id, claimable_contract) in claimable_contracts {
            if wallet
                .claim_contract(federation_id, claimable_contract.clone())
                .await
                .is_ok()
            {
                claimed_contracts.push((machine_id, federation_id, claimable_contract));
            }
        }

        // 3. Remove the successfully transferred payments from the iroh doc.
        if !claimed_contracts.is_empty() {
            let contract_count = claimed_contracts.len();

            let _ = manager_protocol
                .remove_claimable_contracts(claimed_contracts)
                .await;

            tracing::info!("Transferred {contract_count} payment(s) from iroh doc");
        }
    }
}
