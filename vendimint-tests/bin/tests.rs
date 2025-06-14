use std::time::Duration;

use bitcoin::Network;
use fedimint_core::{Amount, invite_code::InviteCode};
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use fedimint_lnv2_remote_client::FinalRemoteReceiveOperationState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    devimint::run_devfed_test()
        .call(|dev_fed, _process_mgr| async move {
            println!("Starting devimint test...");

            let fed = dev_fed.fed().await?;

            println!("Pegging in gateways...");
            fed.pegin_gateways(
                1_000_000,
                vec![
                    dev_fed.gw_lnd().await.unwrap(),
                    dev_fed.gw_ldk().await.unwrap(),
                ],
            )
            .await?;

            println!("Starting vendimint machine...");
            let machine_storage_path = tempfile::tempdir()?;
            let machine =
                vendimint::Machine::new(machine_storage_path.path(), Network::Regtest).await?;

            println!("Starting vendimint manager...");
            let manager_storage_path = tempfile::tempdir()?;
            let manager =
                vendimint::Manager::new(manager_storage_path.path(), Network::Regtest).await?;

            println!("Claiming machine from manager...");
            let (manager_claim_pin, manager_claim_accepter) =
                manager.claim_machine(machine.node_addr().await?).await?;

            let (machine_claim_pin, machine_claim_accepter) =
                machine.await_next_incoming_claim_request().await?;

            assert_eq!(machine_claim_pin, manager_claim_pin);
            assert!(machine_claim_accepter.send(true).is_ok());
            assert!(manager_claim_accepter.send(true).is_ok());

            let federation_invite_code: InviteCode = fed.invite_code()?.parse()?;

            println!("Manager joining federation...");
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
                    .unwrap(),
            };

            println!("Configuring machine...");
            manager
                .set_machine_config(&machine_id, &machine_config)
                .await?;

            // Wait for the claim to sync.
            // TODO: Wait more intelligently.
            tokio::time::sleep(Duration::from_secs(5)).await;

            println!("Machine generating invoice...");
            let (invoice, final_state_receiver) = machine
                .receive_payment(
                    Amount::from_sats(1000),
                    60,
                    Bolt11InvoiceDescription::Direct("Cherry OliPop".to_string()),
                    Some(dev_fed.gw_ldk().await?.addr.parse()?),
                )
                .await?;

            println!("Paying invoice...");
            dev_fed
                .lnd()
                .await?
                .pay_bolt11_invoice(invoice.to_string())
                .await?;

            assert_eq!(
                final_state_receiver.await?,
                FinalRemoteReceiveOperationState::Funded
            );

            println!("Shutting down machine and manager...");
            machine.shutdown().await?;
            manager.shutdown().await?;

            println!("Successfully completed devimint test!");

            Ok(())
        })
        .await
}
