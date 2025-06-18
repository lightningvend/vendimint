mod machine;
mod manager;
mod shared;

pub use machine::MachineProtocol;
pub use manager::ManagerProtocol;
pub use shared::MachineConfig;

#[cfg(test)]
mod tests {
    use super::*;

    use std::{str::FromStr, time::Duration};

    use fedimint_core::{
        Amount, OutPoint, TransactionId, config::FederationId, invite_code::InviteCode,
    };
    use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
    use fedimint_lnv2_remote_client::ClaimableContract;
    use tpe::{AggregatePublicKey, G1Affine};

    // TODO: Cleanup this test, and also test the protocols more thoroughly.
    #[tokio::test]
    async fn test_protocols() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;

        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        let pk = fedimint_core::secp256k1::PublicKey::from_str(
            "03e7798ad2ded4e6dbc6a5a6a891dcb577dadf96842fe500ac46ed5f623aa9042b",
        )?;

        let machine_addr = machine_protocol.node_addr().await?;
        let manager_task = tokio::spawn(async move {
            let (pin_mgr, tx_mgr) = manager_protocol.claim_machine(machine_addr).await.unwrap();
            tx_mgr.send(true).unwrap();
            (pin_mgr, manager_protocol)
        });
        let machine_task = tokio::spawn(async move {
            let (pin_machine, tx_machine) = machine_protocol
                .await_next_incoming_claim_request()
                .await
                .unwrap();
            tx_machine.send(true).unwrap();
            (pin_machine, machine_protocol)
        });
        let ((pin_mgr, manager_protocol), (pin_machine, machine_protocol)) =
            tokio::try_join!(manager_task, machine_task)?;
        assert_eq!(pin_mgr, pin_machine);

        // Wait for claim to finish.
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
            claimer_pk: pk,
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

            assert!(
                i != 9,
                "Timeout waiting for federation invite code to be set"
            );

            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        assert_eq!(
            machine_protocol.get_machine_config().await?,
            Some(machine_config)
        );

        let dummy_federation_id = FederationId::dummy();

        let dummy_claimable_contract = ClaimableContract {
            contract: IncomingContract::new(
                AggregatePublicKey(G1Affine::identity()),
                [127; 32],
                [255; 32],
                PaymentImage::Point(pk),
                Amount { msats: 1234 },
                5678,
                pk,
                pk,
                pk,
            ),
            outpoint: OutPoint {
                txid: TransactionId::from_raw_hash(dummy_federation_id.0),
                out_idx: 0,
            },
        };

        machine_protocol
            .write_payment_to_machine_doc(&dummy_federation_id, &dummy_claimable_contract)
            .await?;

        // Wait for the machine protocol to receive the invite code.
        for i in 0..10 {
            if manager_protocol.get_claimable_contracts().await.is_ok() {
                break;
            }

            assert!(i != 9, "Timeout waiting for claimable contracts to be set");

            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        let claimable_contracts = manager_protocol.get_claimable_contracts().await?;
        assert_eq!(manager_protocol.get_claimable_contracts().await?.len(), 1);

        manager_protocol
            .remove_claimable_contracts(claimable_contracts)
            .await?;

        let claimable_contracts = manager_protocol.get_claimable_contracts().await?;
        assert!(claimable_contracts.is_empty());

        Ok(())
    }

    // Ensure that a machine can only be claimed by a single manager. A subsequent
    // claim attempt from another manager should fail and no claim request should
    // be produced by the machine.
    #[tokio::test]
    async fn test_machine_can_only_be_claimed_once() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;

        let manager1_storage_path = tempfile::tempdir()?;
        let manager1_protocol = ManagerProtocol::new(manager1_storage_path.path()).await?;

        let manager2_storage_path = tempfile::tempdir()?;
        let manager2_protocol = ManagerProtocol::new(manager2_storage_path.path()).await?;

        let machine_addr = machine_protocol.node_addr().await?;

        // First claim succeeds
        let manager_task = tokio::spawn({
            let machine_addr = machine_addr.clone();
            async move {
                let (pin_mgr, tx_mgr) =
                    manager1_protocol.claim_machine(machine_addr).await.unwrap();
                tx_mgr.send(true).unwrap();
                (pin_mgr, manager1_protocol)
            }
        });
        let machine_task = tokio::spawn(async move {
            let (pin_machine, tx_machine) = machine_protocol
                .await_next_incoming_claim_request()
                .await
                .unwrap();
            tx_machine.send(true).unwrap();
            (pin_machine, machine_protocol)
        });
        let ((pin_mgr, manager1_protocol), (pin_machine, machine_protocol)) =
            tokio::try_join!(manager_task, machine_task)?;
        assert_eq!(pin_mgr, pin_machine);

        for _ in 0..10 {
            if manager1_protocol.list_machines().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(manager1_protocol.list_machines()?.len(), 1);

        // Second manager attempts to claim
        let machine_addr2 = machine_protocol.node_addr().await?;
        let (_pin_mgr2, tx_mgr2) = manager2_protocol.claim_machine(machine_addr2).await?;
        tx_mgr2.send(true).unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(manager2_protocol.list_machines()?.len(), 0);

        Ok(())
    }

    // If the machine rejects a claim by sending `false`, no machine should be
    // stored by the manager and the machine should remain unclaimed.
    #[tokio::test]
    async fn test_claim_rejected_by_machine() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;

        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        let machine_addr = machine_protocol.node_addr().await?;

        let manager_task = tokio::spawn(async move {
            let (pin_mgr, tx_mgr) = manager_protocol.claim_machine(machine_addr).await.unwrap();
            tx_mgr.send(true).unwrap();
            (pin_mgr, manager_protocol)
        });
        let machine_task = tokio::spawn(async move {
            let (pin_machine, tx_machine) = machine_protocol
                .await_next_incoming_claim_request()
                .await
                .unwrap();
            tx_machine.send(false).unwrap();
            (pin_machine, machine_protocol)
        });
        let ((pin_mgr, manager_protocol), (_pin_machine, machine_protocol)) =
            tokio::try_join!(manager_task, machine_task)?;
        let _ = pin_mgr;

        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(manager_protocol.list_machines()?.len(), 0);

        // After rejection, another claim should be possible
        let machine_addr = machine_protocol.node_addr().await?;
        let manager_task = tokio::spawn(async move {
            let (pin_mgr, tx_mgr) = manager_protocol.claim_machine(machine_addr).await.unwrap();
            tx_mgr.send(true).unwrap();
            (pin_mgr, manager_protocol)
        });
        let machine_task = tokio::spawn(async move {
            let (pin_machine, tx_machine) = machine_protocol
                .await_next_incoming_claim_request()
                .await
                .unwrap();
            tx_machine.send(true).unwrap();
            (pin_machine, machine_protocol)
        });
        let ((pin_mgr, _manager_protocol), (pin_machine, _machine_protocol)) =
            tokio::try_join!(manager_task, machine_task)?;
        assert_eq!(pin_mgr, pin_machine);

        Ok(())
    }

    // If the manager decides not to claim the machine by sending `false`, the
    // machine will still consider itself claimed by that manager, preventing
    // subsequent claims by others.
    #[tokio::test]
    async fn test_claim_rejected_by_manager() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;

        let manager1_storage_path = tempfile::tempdir()?;
        let manager1_protocol = ManagerProtocol::new(manager1_storage_path.path()).await?;

        let manager2_storage_path = tempfile::tempdir()?;
        let manager2_protocol = ManagerProtocol::new(manager2_storage_path.path()).await?;

        let machine_addr = machine_protocol.node_addr().await?;

        let manager_task = tokio::spawn({
            let machine_addr = machine_addr.clone();
            async move {
                let (pin_mgr, tx_mgr) =
                    manager1_protocol.claim_machine(machine_addr).await.unwrap();
                tx_mgr.send(false).unwrap();
                (pin_mgr, manager1_protocol)
            }
        });
        let machine_task = tokio::spawn(async move {
            let (pin_machine, tx_machine) = machine_protocol
                .await_next_incoming_claim_request()
                .await
                .unwrap();
            tx_machine.send(true).unwrap();
            (pin_machine, machine_protocol)
        });
        let ((_pin_mgr, _manager1_protocol), (_pin_machine, machine_protocol)) =
            tokio::try_join!(manager_task, machine_task)?;

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Second manager should not be able to claim now
        let machine_addr = machine_protocol.node_addr().await?;
        let (_pin2, tx2) = manager2_protocol.claim_machine(machine_addr).await?;
        tx2.send(true).unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(manager2_protocol.list_machines()?.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_restart_persistence() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;

        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        let addr_before = machine_protocol.node_addr().await?;
        let addr_for_mgr = addr_before.clone();
        let manager_task = tokio::spawn(async move {
            let (pin, tx) = manager_protocol.claim_machine(addr_for_mgr).await.unwrap();
            tx.send(true).unwrap();
            (pin, manager_protocol)
        });
        let machine_task = tokio::spawn(async move {
            let (pin, tx) = machine_protocol
                .await_next_incoming_claim_request()
                .await
                .unwrap();
            tx.send(true).unwrap();
            (pin, machine_protocol)
        });
        let ((pin_mgr, manager_protocol), (pin_machine, machine_protocol)) =
            tokio::try_join!(manager_task, machine_task)?;
        assert_eq!(pin_mgr, pin_machine);

        for _ in 0..10 {
            if manager_protocol.list_machines().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        assert_eq!(manager_protocol.list_machines().unwrap().len(), 1);

        machine_protocol.shutdown().await?;
        drop(machine_protocol);

        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;
        let addr_after = machine_protocol.node_addr().await?;

        assert_eq!(addr_before.node_id, addr_after.node_id);

        Ok(())
    }
}
