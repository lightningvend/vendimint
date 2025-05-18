#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Duration};

    use fedimint_core::{Amount, config::FederationId, invite_code::InviteCode};
    use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
    use machine::MachineProtocol;
    use shared::MachineConfig;
    use tpe::{AggregatePublicKey, G1Affine};

    #[tokio::test]
    #[ignore]
    async fn test_machine_protocol() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let mut machine_protocol =
            MachineProtocol::new(machine_storage_path.path().to_path_buf()).await?;

        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol =
            manager::ManagerProtocol::new(manager_storage_path.path().to_path_buf()).await?;

        let pk = fedimint_core::secp256k1::PublicKey::from_str(
            "03e7798ad2ded4e6dbc6a5a6a891dcb577dadf96842fe500ac46ed5f623aa9042b",
        )?;

        let federation_id = FederationId::dummy();

        let incoming_contract = IncomingContract::new(
            AggregatePublicKey(G1Affine::identity()),
            [127; 32],
            [255; 32],
            PaymentImage::Point(pk),
            Amount { msats: 1234 },
            5678,
            pk,
            pk,
            pk,
        );

        machine_protocol
            .write_payment_to_machine_doc(&federation_id, &incoming_contract)
            .await?;

        let machine_addr = machine_protocol.node_addr().await?;
        let (pin_mgr, tx_mgr) = manager_protocol.claim_machine(machine_addr).await?;
        let (pin_machine, tx_machine) = machine_protocol.await_next_incoming_claim_request().await?;
        assert_eq!(pin_mgr, pin_machine);
        tx_mgr.send(true).unwrap();
        tx_machine.send(true).unwrap();

        // wait for claim to finish
        for _ in 0..10 {
            if manager_protocol.list_machines().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        assert_eq!(manager_protocol.list_machines().unwrap().len(), 1);

        assert_eq!(machine_protocol.get_machine_config().await?, None);

        let federation_invite_code = InviteCode::from_str("fed11qgqpcxnhwden5te0vejkg6tdd9h8gepwd4cxcuewvdshx6p0qvqjpypneenvnkhq0actdl9e4l72ah5gel78dylu5wkc9d3kyy52f62asrl562").unwrap();

        let machine_config = MachineConfig {
            federation_invite_code,
        };

        let machine_id = manager_protocol.list_machines().unwrap()[0].0;
        manager_protocol
            .set_machine_config(&machine_id, &machine_config)
            .await?;

        // Wait for the machine protocol to receive the invite code.
        for i in 0..10 {
            if let Ok(Some(_)) = machine_protocol.get_machine_config().await {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;

            if i == 9 {
                panic!("Timeout waiting for federation invite code to be set");
            }
        }

        assert_eq!(
            machine_protocol.get_machine_config().await?,
            Some(machine_config)
        );

        Ok(())
    }
}
