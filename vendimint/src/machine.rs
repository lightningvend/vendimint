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
use lightning_invoice::Bolt11Invoice;
use tokio::sync::oneshot;

const PROTOCOL_SUBDIR: &str = "protocol";
const FEDIMINT_SUBDIR: &str = "fedimint";

pub struct Machine {
    iroh_protocol: Arc<MachineProtocol>,
    wallet: Arc<Wallet>,

    /// Task which moves claimable payments from `Self.wallet` to `Self.machine_protocol`.
    /// This is safe to cancel/abort at any time, since it copies payments over to the new
    /// location before deleting them from the old location, and all write operations are
    /// idempotent.
    syncer_task_handle: tokio::task::JoinHandle<()>,
}

impl Machine {
    pub async fn new(storage_path: &Path, network: Network) -> anyhow::Result<Self> {
        let network_partitioned_storage_path = storage_path.join(network.to_string());

        let iroh_protocol = Arc::new(
            MachineProtocol::new(&network_partitioned_storage_path.join(PROTOCOL_SUBDIR)).await?,
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
                if let Ok(Some(machine_config)) = iroh_protocol_clone.get_machine_config().await {
                    // Handle any change in the machine config.
                    if wallet_clone
                        .join_federation(machine_config.federation_invite_code.clone())
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
            syncer_task_handle,
        })
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.iroh_protocol.shutdown().await?;

        // This task is safe to abort. See the property's documentation for why this is the case.
        self.syncer_task_handle.abort();

        Ok(())
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.iroh_protocol.is_shutdown() && self.syncer_task_handle.is_finished()
    }

    pub async fn node_addr(&self) -> anyhow::Result<NodeAddr> {
        self.iroh_protocol.node_addr().await
    }

    pub async fn get_machine_config(&self) -> anyhow::Result<Option<MachineConfig>> {
        self.iroh_protocol.get_machine_config().await
    }

    pub async fn await_next_incoming_claim_request(
        &self,
    ) -> anyhow::Result<(u32, oneshot::Sender<bool>)> {
        self.iroh_protocol.await_next_incoming_claim_request().await
    }

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

    pub async fn await_receive_payment_final_state(
        &self,
        operation_id: OperationId,
    ) -> anyhow::Result<FinalRemoteReceiveOperationState> {
        self.wallet
            .await_receive_payment_final_state(operation_id)
            .await
    }

    /// Make completed payments syncable to the manager.
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
}
