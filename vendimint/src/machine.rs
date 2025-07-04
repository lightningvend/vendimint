use std::{path::Path, sync::Arc, time::Duration};

use crate::fedimint_wallet::Wallet;
use crate::vendimint_iroh::MachineConfig;
use crate::vendimint_iroh::MachineProtocol;
use bitcoin::Network;
use fedimint_client::OperationId;
use fedimint_core::{Amount, util::SafeUrl};
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use fedimint_lnv2_remote_client::FinalRemoteReceiveOperationState;
use iroh::NodeAddr;
use iroh_docs::sync::Entry;
use lightning_invoice::Bolt11Invoice;
use tokio::sync::oneshot;

const PROTOCOL_SUBDIR: &str = "protocol";
const FEDIMINT_SUBDIR: &str = "fedimint";

/// A device/application that receives funds (e.g., vending machine, point-of-sale).
pub struct Machine {
    iroh_protocol: Arc<MachineProtocol>,
    wallet: Arc<Wallet>,

    /// Task which moves claimable payments from `Self.wallet` to `Self.iroh_protocol`.
    /// This is safe to cancel/abort at any time, since it copies payments over to the new
    /// location before deleting them from the old location, and all write operations are
    /// idempotent. Is `None` after `Self::shutdown` has been called, otherwise is `Some`.
    syncer_task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Machine {
    /// Creates a new machine instance.
    ///
    /// The storage path provided should not contain any data
    /// other than that which was written by the machine. It
    /// also should not have more than one machine interacting
    /// with files in the storage path concurrently. Both of
    /// these cases are undefined behavior.
    ///
    /// For a given storage path, switching between different
    /// networks is safe. Data is partitioned by network, so
    /// each network has its own isolated storage.
    pub async fn new(storage_path: &Path, network: Network) -> anyhow::Result<Self> {
        let network_partitioned_storage_path = storage_path.join(network.to_string());

        let iroh_protocol = Arc::new(
            MachineProtocol::new(&network_partitioned_storage_path.join(PROTOCOL_SUBDIR)).await?,
        );

        let wallet = Arc::new(
            Wallet::new(
                network_partitioned_storage_path.join(FEDIMINT_SUBDIR),
                network,
            )
            .await?,
        );

        let iroh_protocol_clone = iroh_protocol.clone();
        let wallet_clone = wallet.clone();
        let syncer_task_handle = tokio::task::spawn(async move {
            loop {
                if let Ok(Some(machine_config)) = iroh_protocol_clone.get_machine_config().await {
                    // Handle any change in the machine config.
                    if wallet_clone
                        .set_default_federation(machine_config.federation_invite_code.clone())
                        .await
                        .is_err()
                    {
                        continue;
                    }

                    Self::make_completed_payments_syncable(
                        machine_config,
                        &iroh_protocol_clone,
                        &wallet_clone,
                    )
                    .await;
                }

                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });

        Ok(Self {
            iroh_protocol,
            wallet,
            syncer_task_handle: Some(syncer_task_handle),
        })
    }

    /// Shuts down the machine idempotently.
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

    /// Checks if the machine is already shut down.
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.iroh_protocol.is_shutdown() && self.syncer_task_handle.is_none()
    }

    /// Gets the network address of the machine, which can be exchanged
    /// out-of-band and passed into [`crate::Manager::claim_machine`].
    pub async fn node_addr(&self) -> anyhow::Result<NodeAddr> {
        self.iroh_protocol.node_addr().await
    }

    /// Gets the machine's configuration, which is automatically configured by its manager.
    /// Returns `Some` if the machine is configured, `None` otherwise.
    pub async fn get_machine_config(&self) -> anyhow::Result<Option<MachineConfig>> {
        self.iroh_protocol.get_machine_config().await
    }

    /// Awaits the next request from a manager to claim the machine.
    ///
    /// Always immediately returns `None` after [`Self::shutdown`] has
    /// been called. Waits forever if the machine is already claimed
    /// (or until shutdown).
    ///
    /// If the machine is not already claimed, any incoming claim request
    /// will wait for this method to be called. For this reason, **it is
    /// important to routinely call this method** (until it returns `None`),
    /// ideally in a loop, to ensure quick alerting of any incoming claim
    /// requests.
    ///
    /// When `Some` is returned, it will contain the claim ID which should be
    /// presented to the user to compare with the claim ID displayed on the
    /// manager. This is to prevent eavesdropping/claim sniping. It will also
    /// contain a `oneshot::Sender<bool>` which can be used to respond to the
    /// claim request. Sending `true` will accept the claim, and sending `false`
    /// or dropping the sender will reject it. If the machine and the manager
    /// both accept the claim after verifying matching claim IDs, the claim will
    /// be accepted by both parties. If either one rejects the claim, it will be
    /// aborted by both parties.
    pub async fn await_next_incoming_claim_request(&self) -> Option<(u32, oneshot::Sender<bool>)> {
        self.iroh_protocol.await_next_incoming_claim_request().await
    }

    /// Creates a [Bolt11](https://github.com/lightning/bolts/blob/master/11-payment-encoding.md)
    /// lightning invoice and an operation ID which can be used to await the payment/timeout
    /// of the invoice by passing it into [`Self::await_receive_payment_final_state`].
    ///
    /// If the invoice is paid, the funds will be automatically credited to the wallet of the
    /// manager that claimed the machine whenever both the machine and the manager are online
    /// at the same time. While the machine knows when the invoice is paid, it never has
    /// access to the funds.
    pub async fn receive_payment(
        &self,
        amount: Amount,
        expiry_secs: u32,
        description: Bolt11InvoiceDescription,
        gateway: Option<SafeUrl>,
    ) -> anyhow::Result<(Bolt11Invoice, OperationId)> {
        let Some(machine_config) = self.iroh_protocol.get_machine_config().await? else {
            return Err(anyhow::anyhow!(
                "Machine cannot accept payments as it is not configured"
            ));
        };

        self.wallet
            .receive_payment(
                machine_config.federation_invite_code.federation_id(),
                machine_config.claimer_pk,
                amount,
                expiry_secs,
                description,
                gateway,
            )
            .await
    }

    /// Awaits the final state of an invoice created by [`Self::receive_payment`].
    pub async fn await_receive_payment_final_state(
        &self,
        operation_id: OperationId,
    ) -> anyhow::Result<FinalRemoteReceiveOperationState> {
        self.wallet
            .await_receive_payment_final_state(operation_id)
            .await
    }

    /// Makes completed payments syncable to the manager.
    /// This must be called periodically to ensure that
    /// the manager is able to sweep completed payments.
    async fn make_completed_payments_syncable(
        machine_config: MachineConfig,
        machine_protocol: &Arc<MachineProtocol>,
        wallet: &Arc<Wallet>,
    ) {
        let federation_id = machine_config.federation_invite_code.federation_id();
        let claimer_pk = machine_config.claimer_pk;

        // 1. Fetch completed payments from the
        // wallet that the manager could sweep.
        let Some(claimable_contracts) = wallet
            .get_claimable_contracts(federation_id, claimer_pk, None)
            .await
        else {
            return;
        };

        // 2. Write the sweepable payments to the iroh doc
        // that this machine shares with the manager.
        let mut contract_ids = Vec::new();
        for claimable_contract in claimable_contracts {
            if machine_protocol
                .write_payment_to_machine_doc(&federation_id, &claimable_contract)
                .await
                .is_ok()
            {
                contract_ids.push(claimable_contract.contract.contract_id());
            }
        }

        // 3. Remove the successfully transferred payments from the wallet.
        if !contract_ids.is_empty() {
            let contract_count = contract_ids.len();

            wallet
                .remove_claimed_contracts(federation_id, contract_ids)
                .await;

            tracing::info!("Transferred {contract_count} payment(s) to iroh doc");
        }
    }

    /// Get a key/value entry from the machine's shared storage.
    ///
    /// KV data is stored on an auto-syncing key/value store shared
    /// between a machine and its manager. It has no meaning within
    /// the context of vendimint - its meaning is up to the API caller.
    pub async fn get_kv_value(&self, key: impl AsRef<[u8]>) -> anyhow::Result<Option<Entry>> {
        self.iroh_protocol.get_kv_value(key).await
    }

    /// Set a key/value entry in the machine's shared storage.
    ///
    /// KV data is stored on an auto-syncing key/value store shared
    /// between a machine and its manager. It has no meaning within
    /// the context of vendimint - its meaning is up to the API caller.
    pub async fn set_kv_value(
        &self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
    ) -> anyhow::Result<()> {
        self.iroh_protocol.set_kv_value(key, value).await
    }

    /// Get all key/value entries from the machine's shared storage.
    ///
    /// KV data is stored on an auto-syncing key/value store shared
    /// between a machine and its manager. It has no meaning within
    /// the context of vendimint - its meaning is up to the API caller.
    pub async fn get_kv_entries(&self) -> anyhow::Result<Vec<Entry>> {
        self.iroh_protocol.get_kv_entries().await
    }
}
