use std::{path::Path, sync::Arc, time::Duration};

use crate::fedimint_wallet::Wallet;
use crate::vendimint_iroh::MachineConfig;
use crate::vendimint_iroh::ManagerProtocol;
use bitcoin::Network;
use fedimint_core::Amount;
use fedimint_core::{config::FederationId, invite_code::InviteCode};
use fedimint_mint_client::OOBNotes;
use iroh::{NodeAddr, NodeId};
use serde::Serialize;
use tokio::sync::oneshot;

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
    /// Creates a new manager instance.
    ///
    /// The storage path provided should not contain any data
    /// other than that which was written by the manager. It
    /// also should not have more than one manager interacting
    /// with files in the storage path concurrently. Both of
    /// these cases are undefined behavior.
    ///
    /// For a given storage path, switching between different
    /// networks is safe. Data is partitioned by network, so
    /// each network has its own isolated storage.
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
                Self::update_machine_configs(&iroh_protocol_clone, &wallet_clone).await;

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

    /// Checks if the manager is already shut down.
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.iroh_protocol.is_shutdown() && self.syncer_task_handle.is_none()
    }

    /// Claims a machine by connecting to it via its node address.
    ///
    /// Returns a claim ID and a sender to accept/reject the claim.
    /// The claim ID should be verified against the one displayed on the machine
    /// before accepting to prevent claim sniping. Once verified, send `true` to
    /// the sender to accept the claim, or `false` to reject it.
    pub async fn claim_machine(
        &self,
        node_addr: NodeAddr,
    ) -> anyhow::Result<(u32, oneshot::Sender<bool>)> {
        self.iroh_protocol.claim_machine(node_addr).await
    }

    /// Lists the node IDs of all machines claimed by this manager.
    pub fn list_machine_ids(&self) -> std::io::Result<Vec<NodeId>> {
        Ok(self
            .iroh_protocol
            .list_machines()?
            .into_iter()
            .map(|(machine_id, _)| machine_id)
            .collect())
    }

    /// Gets the configuration for a specific machine.
    ///
    /// Returns `Some` if the machine is configured, `None` otherwise.
    pub async fn get_machine_config(
        &self,
        machine_id: &NodeId,
    ) -> anyhow::Result<Option<MachineConfig>> {
        self.iroh_protocol.get_machine_config(machine_id).await
    }

    /// Updates the default federation invite and configures all claimed machines.
    ///
    /// This automatically syncs every claimed machine to the default federation
    /// and must be called at least once for machines to accept payments.
    pub async fn update_federation(&self, invite_code: InviteCode) -> anyhow::Result<()> {
        self.wallet
            .set_default_federation(invite_code.clone())
            .await?;
        Self::update_machine_configs(&self.iroh_protocol, &self.wallet).await;
        Ok(())
    }

    pub async fn get_local_balance(&self) -> Amount {
        self.wallet.get_local_balance().await
    }

    /// Sweeps all ecash notes from a federation into out-of-band notes.
    ///
    /// Returns `Some` if there were notes to sweep, `None` if the balance was zero.
    /// The operation will be cancelled and the notes reclaimed after the specified
    /// duration if they haven't been claimed elsewhere.
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

    /// Makes completed payments syncable to the manager.
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
            if let Err(err) = wallet
                .claim_contract(federation_id, claimable_contract.clone())
                .await
            {
                // TODO: Should we add a dead-letter queue for failed
                // contracts and also remove them from the iroh doc?
                tracing::error!(
                    "Failed to claim contract for machine {machine_id} and federation {federation_id}\n\nError:\n{err}\n\nContract:\n{claimable_contract:#?}"
                );
            } else {
                // TODO: Test that contracts get removed from the iroh doc.
                // Maybe add test functionality to `Manager` and `Machine`
                // to get a full dump of the iroh doc from both sides, then
                // assert that they're equal, then verify the contents.
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

    async fn update_machine_configs(manager_protocol: &Arc<ManagerProtocol>, wallet: &Arc<Wallet>) {
        let Ok(Some(default_invite)) = wallet.get_default_federation().await else {
            return;
        };

        let federation_id = default_invite.federation_id();

        let Some(claimer_pk) = wallet.get_lnv2_claim_pubkey(federation_id).await else {
            return;
        };

        let Ok(machines) = manager_protocol.list_machines() else {
            return;
        };

        let new_config = MachineConfig {
            federation_invite_code: default_invite,
            claimer_pk,
        };

        for (machine_id, _) in machines {
            let Ok(cfg) = manager_protocol.get_machine_config(&machine_id).await else {
                continue;
            };

            let needs_update = match cfg {
                Some(c) => c.federation_invite_code.federation_id() != federation_id,
                None => true,
            };

            if needs_update {
                let _ = manager_protocol
                    .set_machine_config(&machine_id, &new_config)
                    .await;
            }
        }
    }
}
