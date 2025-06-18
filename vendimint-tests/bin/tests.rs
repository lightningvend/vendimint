use std::time::Duration;

use bitcoin::Network;
use fedimint_core::{Amount, invite_code::InviteCode};
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use fedimint_lnv2_remote_client::FinalRemoteReceiveOperationState;

// TODO: Split up code better so we don't need this clippy rule exemption.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    devimint::run_devfed_test()
        .call(|dev_fed, _process_mgr| async move {
            tracing::info!("Starting devimint test...");

            let fed = dev_fed.fed().await?;

            tracing::info!("Pegging in gateways...");
            fed.pegin_gateways(
                1_000_000,
                vec![
                    dev_fed.gw_lnd().await.unwrap(),
                    dev_fed.gw_ldk().await.unwrap(),
                ],
            )
            .await?;

            tracing::info!("Starting vendimint machine...");
            let machine_storage_path = tempfile::tempdir()?;
            let mut machine =
                vendimint::Machine::new(machine_storage_path.path(), Network::Regtest).await?;

            tracing::info!("Starting vendimint manager...");
            let manager_storage_path = tempfile::tempdir()?;
            let mut manager =
                vendimint::Manager::new(manager_storage_path.path(), Network::Regtest).await?;

            tracing::info!("Claiming machine from manager...");
            let (manager_claim_pin, manager_claim_accepter) =
                manager.claim_machine(machine.node_addr().await?).await?;

            let (machine_claim_pin, machine_claim_accepter) =
                machine.await_next_incoming_claim_request().await?;

            assert_eq!(machine_claim_pin, manager_claim_pin);
            assert!(machine_claim_accepter.send(true).is_ok());
            assert!(manager_claim_accepter.send(true).is_ok());

            let federation_invite_code: InviteCode = fed.invite_code()?.parse()?;
            let federation_id = federation_invite_code.federation_id();

            tracing::info!("Manager joining federation...");
            manager
                .join_federation(federation_invite_code.clone())
                .await?;

            // Wait for the claim to sync.
            // TODO: Wait more intelligently.
            tokio::time::sleep(Duration::from_secs(5)).await;

            let machine_ids = manager.list_machine_ids()?;

            assert_eq!(machine_ids.len(), 1);
            let machine_id = machine_ids[0];

            let machine_config = vendimint::MachineConfig {
                federation_invite_code,
                claimer_pk: manager
                    .get_fedimint_lnv2_claim_pubkey(fed.calculate_federation_id().parse()?)
                    .await
                    .unwrap(),
            };

            tracing::info!("Configuring machine...");
            manager
                .set_machine_config(&machine_id, &machine_config)
                .await?;

            // Wait for the claim to sync.
            // TODO: Wait more intelligently.
            tokio::time::sleep(Duration::from_secs(5)).await;

            assert_eq!(machine.get_machine_config().await?, Some(machine_config));

            tracing::info!("Machine generating invoice...");
            let (invoice, operation_id) = machine
                .receive_payment(
                    Amount::from_sats(1_000),
                    60,
                    Bolt11InvoiceDescription::Direct("Cherry OliPop".to_string()),
                    Some(dev_fed.gw_ldk().await?.addr.parse()?),
                )
                .await?;

            tracing::info!("Paying invoice...");
            dev_fed
                .lnd()
                .await?
                .pay_bolt11_invoice(invoice.to_string())
                .await?;

            tracing::info!("Waiting for payment to be received to machine...");
            let final_payment_state = machine
                .await_receive_payment_final_state(operation_id)
                .await?;

            assert_eq!(
                final_payment_state,
                FinalRemoteReceiveOperationState::Funded
            );

            tracing::info!("Extracting ecash from manager...");
            let mut i = 0;
            let ecash = loop {
                if let Some(ecash) = manager
                    .sweep_all_ecash_notes(
                        federation_id,
                        Duration::from_secs(30),
                        false,
                        None::<()>,
                    )
                    .await?
                {
                    break ecash;
                }

                i += 1;
                assert!(i <= 100, "Timeout waiting for ecash");
                tokio::time::sleep(Duration::from_secs(1)).await;
            };

            // Original 1,000 sat payment, minus federation and gateway fees.
            assert_eq!(ecash.total_amount(), Amount::from_msats(943_906));

            tracing::info!("Extracted manager ecash balance: {}", ecash.total_amount());

            tracing::info!("Shutting down machine and manager...");
            assert!(!machine.is_shutdown());
            machine.shutdown().await?;
            assert!(machine.is_shutdown());
            assert!(!manager.is_shutdown());
            manager.shutdown().await?;
            assert!(manager.is_shutdown());

            // Ensure shutdown commands are idempotent.
            machine.shutdown().await?;
            assert!(machine.is_shutdown());
            manager.shutdown().await?;
            assert!(manager.is_shutdown());

            tracing::info!("Successfully completed devimint test!");

            Ok(())
        })
        .await
}
