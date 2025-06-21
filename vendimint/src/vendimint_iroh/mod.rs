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
    use tokio::time::Instant;
    use tpe::{AggregatePublicKey, G1Affine};

    const IROH_WAIT_DELAY: Duration = Duration::from_millis(100);

    /// Helper to create a machine and manager protocol pair with temp directories
    ///
    /// NOTE: The TempDir objects MUST be returned and kept alive by the caller!
    /// The protocols only store PathBuf internally, not the TempDir itself, so if
    /// we don't return the TempDir objects, they get dropped at the end of this
    /// function and the directories are deleted, causing "No such file or directory"
    /// errors when the protocols try to access their storage.
    async fn create_machine_manager_pair() -> anyhow::Result<(
        MachineProtocol,
        ManagerProtocol,
        tempfile::TempDir,
        tempfile::TempDir,
    )> {
        let machine_temp = tempfile::tempdir()?;
        let machine_protocol = MachineProtocol::new(machine_temp.path()).await?;

        let manager_temp = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_temp.path()).await?;

        Ok((
            machine_protocol,
            manager_protocol,
            machine_temp,
            manager_temp,
        ))
    }

    /// Helper to perform a claim between machine and manager.
    async fn perform_claim(
        machine_protocol: MachineProtocol,
        manager_protocol: ManagerProtocol,
        machine_accepts: bool,
        manager_accepts: bool,
    ) -> anyhow::Result<(u32, MachineProtocol, ManagerProtocol)> {
        let machine_addr = machine_protocol.node_addr().await?;
        let manager_task = tokio::spawn(async move {
            let (pin_mgr, tx_mgr) = manager_protocol.claim_machine(machine_addr).await.unwrap();
            tx_mgr.send(manager_accepts).unwrap();
            (pin_mgr, manager_protocol)
        });
        let machine_task = tokio::spawn(async move {
            let (pin_machine, tx_machine) = machine_protocol
                .await_next_incoming_claim_request()
                .await
                .unwrap();
            tx_machine.send(machine_accepts).unwrap();
            (pin_machine, machine_protocol)
        });
        let ((pin_mgr, manager_protocol), (pin_machine, machine_protocol)) =
            tokio::try_join!(manager_task, machine_task)?;
        assert_eq!(pin_mgr, pin_machine);

        Ok((pin_mgr, machine_protocol, manager_protocol))
    }

    /// Helper to wait for a condition with timeout
    async fn wait_for_condition<F, Fut>(
        mut condition: F,
        timeout_iterations: u32,
    ) -> anyhow::Result<()>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..timeout_iterations {
            if condition().await {
                return Ok(());
            }
            tokio::time::sleep(IROH_WAIT_DELAY).await;
        }
        Err(anyhow::anyhow!("Condition timeout"))
    }

    /// Helper to create a machine-manager pair that are paired and ready for testing.
    ///
    /// This combines the common pattern of creating protocols, performing a claim,
    /// and waiting for the claim to complete. Returns the claimed pair ready for
    /// testing specific functionality.
    async fn create_claimed_machine_manager_pair() -> anyhow::Result<(
        MachineProtocol,
        ManagerProtocol,
        tempfile::TempDir,
        tempfile::TempDir,
    )> {
        let (machine_protocol, manager_protocol, machine_temp, manager_temp) =
            create_machine_manager_pair().await?;

        let (_pin, machine_protocol, manager_protocol) =
            perform_claim(machine_protocol, manager_protocol, true, true).await?;

        // Wait for claim to finish
        wait_for_condition(
            || async { manager_protocol.list_machines().unwrap().len() == 1 },
            10,
        )
        .await?;

        Ok((
            machine_protocol,
            manager_protocol,
            machine_temp,
            manager_temp,
        ))
    }

    /// Helper to create test claimable contract
    fn create_test_claimable_contract(
        pk: fedimint_core::secp256k1::PublicKey,
    ) -> (FederationId, ClaimableContract) {
        create_test_claimable_contract_with_federation_id(pk, FederationId::dummy())
    }

    /// Helper to create test claimable contract with specific federation ID
    fn create_test_claimable_contract_with_federation_id(
        pk: fedimint_core::secp256k1::PublicKey,
        federation_id: FederationId,
    ) -> (FederationId, ClaimableContract) {
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
                txid: TransactionId::from_raw_hash(federation_id.0),
                out_idx: 0,
            },
        };
        (federation_id, dummy_claimable_contract)
    }

    /// Helper to get test public key
    fn get_test_public_key() -> anyhow::Result<fedimint_core::secp256k1::PublicKey> {
        fedimint_core::secp256k1::PublicKey::from_str(
            "03e7798ad2ded4e6dbc6a5a6a891dcb577dadf96842fe500ac46ed5f623aa9042b",
        )
        .map_err(Into::into)
    }

    /// Helper to create a dummy NodeId for testing
    fn create_dummy_node_id() -> iroh::NodeId {
        // Use a dummy string that represents a valid NodeId
        let dummy_hex = "0101010101010101010101010101010101010101010101010101010101010101";
        iroh::NodeId::from_str(dummy_hex).unwrap()
    }

    #[tokio::test]
    async fn test_basic_claim_flow() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(manager_protocol.get_public_key().await?)
        );
        assert_eq!(manager_protocol.list_machines().unwrap().len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_config_operations() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        assert_eq!(machine_protocol.get_machine_config().await?, None);

        let pk = get_test_public_key()?;
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
        wait_for_condition(
            || async { matches!(machine_protocol.get_machine_config().await, Ok(Some(_))) },
            10,
        )
        .await?;

        assert_eq!(
            machine_protocol.get_machine_config().await?,
            Some(machine_config)
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_claimable_contract_operations() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let pk = get_test_public_key()?;
        let (dummy_federation_id, dummy_claimable_contract) = create_test_claimable_contract(pk);

        machine_protocol
            .write_payment_to_machine_doc(&dummy_federation_id, &dummy_claimable_contract)
            .await?;

        // Wait for the manager protocol to sync the claimable contracts.
        wait_for_condition(
            || async { manager_protocol.get_claimable_contracts().await.is_ok() },
            10,
        )
        .await?;

        let claimable_contracts = manager_protocol.get_claimable_contracts().await?;
        assert_eq!(claimable_contracts.len(), 1);

        manager_protocol
            .remove_claimable_contracts(claimable_contracts)
            .await?;

        let claimable_contracts = manager_protocol.get_claimable_contracts().await?;
        assert!(claimable_contracts.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_claim_requests_rejected_after_shutdown() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;

        machine_protocol.shutdown().await?;

        // After machine shutdown, claim requests should immediately return `None`.
        let start = Instant::now();
        for _ in 0..10 {
            assert!(
                machine_protocol
                    .await_next_incoming_claim_request()
                    .await
                    .is_none()
            );
        }
        assert!(start.elapsed() < Duration::from_millis(10));

        Ok(())
    }

    // Ensure that a machine can only be claimed by a single manager. A subsequent
    // claim attempt from another manager should fail and no claim request should
    // be produced by the machine.
    #[tokio::test]
    async fn test_machine_can_only_be_claimed_once() -> anyhow::Result<()> {
        let (machine_protocol, manager1_protocol, _machine_temp, _manager1_temp) =
            create_machine_manager_pair().await?;

        let manager2_storage_path = tempfile::tempdir()?;
        let manager2_protocol = ManagerProtocol::new(manager2_storage_path.path()).await?;

        // First claim succeeds
        let (_pin, machine_protocol, manager1_protocol) =
            perform_claim(machine_protocol, manager1_protocol, true, true).await?;

        wait_for_condition(
            || async { manager1_protocol.list_machines().unwrap().len() == 1 },
            10,
        )
        .await?;
        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(manager1_protocol.get_public_key().await?)
        );
        assert_eq!(manager1_protocol.list_machines()?.len(), 1);

        // Second manager attempts to claim
        let machine_addr2 = machine_protocol.node_addr().await?;
        let (_pin_mgr2, tx_mgr2) = manager2_protocol.claim_machine(machine_addr2).await?;
        tx_mgr2.send(true).unwrap();

        tokio::time::sleep(IROH_WAIT_DELAY).await;

        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(manager1_protocol.get_public_key().await?)
        );
        assert_eq!(manager2_protocol.list_machines()?.len(), 0);

        Ok(())
    }

    // If the machine rejects a claim by sending `false`, no machine should be
    // stored by the manager and the machine should remain unclaimed.
    #[tokio::test]
    async fn test_claim_rejected_by_machine() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_machine_manager_pair().await?;

        let (_pin, machine_protocol, manager_protocol) =
            perform_claim(machine_protocol, manager_protocol, false, true).await?;

        tokio::time::sleep(IROH_WAIT_DELAY).await;

        assert_eq!(machine_protocol.get_manager_pubkey().await, None);
        assert_eq!(manager_protocol.list_machines()?.len(), 0);

        // After rejection, another claim should be possible.
        let (_pin, machine_protocol, manager_protocol) =
            perform_claim(machine_protocol, manager_protocol, true, true).await?;

        tokio::time::sleep(IROH_WAIT_DELAY).await;

        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(manager_protocol.get_public_key().await?)
        );
        assert_eq!(manager_protocol.list_machines()?.len(), 1);

        Ok(())
    }

    // If the manager decides not to claim the machine by sending `false`, the
    // machine will still consider itself claimed by that manager, preventing
    // subsequent claims by others.
    #[tokio::test]
    async fn test_claim_rejected_by_manager() -> anyhow::Result<()> {
        let (machine_protocol, manager1_protocol, _machine_temp, _manager1_temp) =
            create_machine_manager_pair().await?;

        let manager2_storage_path = tempfile::tempdir()?;
        let manager2_protocol = ManagerProtocol::new(manager2_storage_path.path()).await?;

        let (_pin, machine_protocol, manager1_protocol) =
            perform_claim(machine_protocol, manager1_protocol, true, false).await?;

        tokio::time::sleep(IROH_WAIT_DELAY).await;

        assert_eq!(machine_protocol.get_manager_pubkey().await, None);
        assert_eq!(manager1_protocol.list_machines()?.len(), 0);

        // Second manager should be able to claim, since the first claim was aborted
        let (_pin, machine_protocol, manager2_protocol) =
            perform_claim(machine_protocol, manager2_protocol, true, true).await?;

        tokio::time::sleep(IROH_WAIT_DELAY).await;

        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(manager2_protocol.get_public_key().await?)
        );
        assert_eq!(manager2_protocol.list_machines()?.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_restart_persistence() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;

        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        let addr_before = machine_protocol.node_addr().await?;
        let (_pin, machine_protocol, manager_protocol) =
            perform_claim(machine_protocol, manager_protocol, true, true).await?;

        wait_for_condition(
            || async { manager_protocol.list_machines().unwrap().len() == 1 },
            10,
        )
        .await?;

        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(manager_protocol.get_public_key().await?),
        );
        assert_eq!(manager_protocol.list_machines().unwrap().len(), 1);

        machine_protocol.shutdown().await?;
        drop(machine_protocol);

        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;
        let addr_after = machine_protocol.node_addr().await?;

        assert_eq!(addr_before.node_id, addr_after.node_id);
        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(manager_protocol.get_public_key().await?),
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_is_shutdown_status() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;

        assert!(!machine_protocol.is_shutdown());
        machine_protocol.shutdown().await?;
        assert!(machine_protocol.is_shutdown());

        Ok(())
    }

    #[tokio::test]
    async fn test_manager_list_machines_empty() -> anyhow::Result<()> {
        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        let machines = manager_protocol.list_machines()?;
        assert!(machines.is_empty(), "New manager should have no machines");

        Ok(())
    }

    #[tokio::test]
    async fn test_manager_get_machine_config_nonexistent() -> anyhow::Result<()> {
        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        // Create a dummy NodeId
        let dummy_node_id = create_dummy_node_id();
        let result = manager_protocol.get_machine_config(&dummy_node_id).await;

        assert!(
            result.is_err(),
            "Getting config for nonexistent machine should fail"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_manager_set_machine_config_nonexistent() -> anyhow::Result<()> {
        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        let pk = get_test_public_key()?;
        let federation_invite_code = InviteCode::from_str("fed11qgqpcxnhwden5te0vejkg6tdd9h8gepwd4cxcuewvdshx6p0qvqjpypneenvnkhq0actdl9e4l72ah5gel78dylu5wkc9d3kyy52f62asrl562").unwrap();

        let machine_config = MachineConfig {
            federation_invite_code,
            claimer_pk: pk,
        };

        // Create a dummy NodeId
        let dummy_node_id = create_dummy_node_id();
        let result = manager_protocol
            .set_machine_config(&dummy_node_id, &machine_config)
            .await;

        assert!(
            result.is_err(),
            "Setting config for nonexistent machine should fail"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_manager_get_claimable_contracts_empty() -> anyhow::Result<()> {
        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        let contracts = manager_protocol.get_claimable_contracts().await?;
        assert!(
            contracts.is_empty(),
            "New manager should have no claimable contracts"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_manager_remove_claimable_contracts_empty() -> anyhow::Result<()> {
        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        // Should succeed even with empty list
        manager_protocol.remove_claimable_contracts(vec![]).await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_idempotent_contract_writes() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let pk = get_test_public_key()?;
        let (federation_id, claimable_contract) = create_test_claimable_contract(pk);

        // Write the same contract twice (this should be idempotent)
        machine_protocol
            .write_payment_to_machine_doc(&federation_id, &claimable_contract)
            .await?;
        machine_protocol
            .write_payment_to_machine_doc(&federation_id, &claimable_contract)
            .await?;

        // Wait for sync
        wait_for_condition(
            || async { manager_protocol.get_claimable_contracts().await.is_ok() },
            10,
        )
        .await?;

        let contracts = manager_protocol.get_claimable_contracts().await?;
        assert_eq!(
            contracts.len(),
            1,
            "Should have one contract (idempotent writes)"
        );

        // Verify the contract details
        assert_eq!(contracts[0].1, federation_id);

        Ok(())
    }

    #[tokio::test]
    async fn test_contract_removal() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let pk = get_test_public_key()?;
        let (federation_id, claimable_contract) = create_test_claimable_contract(pk);

        let contracts = manager_protocol.get_claimable_contracts().await?;
        assert_eq!(contracts.len(), 0);

        // Write a contract.
        machine_protocol
            .write_payment_to_machine_doc(&federation_id, &claimable_contract)
            .await?;

        // Wait for contract to sync from machine to manager.
        wait_for_condition(
            || async {
                let contracts = manager_protocol.get_claimable_contracts().await.unwrap();
                contracts.len() == 1
            },
            10,
        )
        .await?;

        // Remove the contract
        let contracts = manager_protocol.get_claimable_contracts().await?;
        manager_protocol
            .remove_claimable_contracts(contracts)
            .await?;

        // Should have no contracts left
        let remaining_contracts = manager_protocol.get_claimable_contracts().await?;
        assert_eq!(remaining_contracts.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_shared_protocol_key_functions() -> anyhow::Result<()> {
        use super::shared::SharedProtocol;

        let pk = get_test_public_key()?;
        let (federation_id, claimable_contract) = create_test_claimable_contract(pk);

        // Test key creation and parsing
        let key = SharedProtocol::create_claimable_contract_machine_doc_key(
            &federation_id,
            &claimable_contract,
        );

        let (parsed_fed_id, parsed_contract_id) =
            SharedProtocol::parse_incoming_contract_machine_doc_key(&key)?;

        assert_eq!(parsed_fed_id, federation_id);
        assert_eq!(
            parsed_contract_id,
            claimable_contract.contract.contract_id()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_shared_protocol_key_parsing_errors() -> anyhow::Result<()> {
        use super::shared::SharedProtocol;

        // Test invalid key length
        let short_key = vec![1, 2, 3];
        let result = SharedProtocol::parse_incoming_contract_machine_doc_key(&short_key);
        assert!(result.is_err(), "Should fail with short key");

        // Test invalid prefix
        let wrong_prefix_key = vec![0u8; 66]; // Right length, wrong prefix
        let result = SharedProtocol::parse_incoming_contract_machine_doc_key(&wrong_prefix_key);
        assert!(result.is_err(), "Should fail with wrong prefix");

        Ok(())
    }

    #[tokio::test]
    async fn test_claim_pin_generation() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_machine_manager_pair().await?;

        let machine_addr = machine_protocol.node_addr().await?;

        // Start claim process to get the pin
        let manager_task =
            tokio::spawn(
                async move { manager_protocol.claim_machine(machine_addr).await.unwrap() },
            );
        let machine_task = tokio::spawn(async move {
            machine_protocol
                .await_next_incoming_claim_request()
                .await
                .unwrap()
        });

        let ((pin_mgr, _tx_mgr), (pin_machine, _tx_machine)) =
            tokio::try_join!(manager_task, machine_task)?;

        // Pins should match and be valid 6-digit numbers
        assert_eq!(pin_mgr, pin_machine);
        assert!(pin_mgr < 1_000_000, "PIN should be less than 1,000,000");

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_config_serialization() -> anyhow::Result<()> {
        let pk = get_test_public_key()?;
        let federation_invite_code = InviteCode::from_str(
            "fed11qgqpcxnhwden5te0vejkg6tdd9h8gepwd4cxcuewvdshx6p0qvqjpypneenvnkhq0actdl9e4l72ah5gel78dylu5wkc9d3kyy52f62asrl562"
        ).unwrap();

        let machine_config = MachineConfig {
            federation_invite_code: federation_invite_code.clone(),
            claimer_pk: pk,
        };

        // Test serialization and deserialization
        let serialized = serde_json::to_string(&machine_config)?;
        let deserialized: MachineConfig = serde_json::from_str(&serialized)?;

        assert_eq!(machine_config, deserialized);
        assert_eq!(
            machine_config.federation_invite_code,
            federation_invite_code
        );
        assert_eq!(machine_config.claimer_pk, pk);

        Ok(())
    }

    #[tokio::test]
    async fn test_concurrent_claim_attempts() -> anyhow::Result<()> {
        let (machine_protocol, manager1_protocol, _machine_temp, _manager1_temp) =
            create_machine_manager_pair().await?;

        let manager2_storage_path = tempfile::tempdir()?;
        let manager2_protocol = ManagerProtocol::new(manager2_storage_path.path()).await?;

        let manager3_storage_path = tempfile::tempdir()?;
        let manager3_protocol = ManagerProtocol::new(manager3_storage_path.path()).await?;

        let machine_addr = machine_protocol.node_addr().await?;

        // Start multiple concurrent claim attempts
        let manager1_task = tokio::spawn({
            let manager1_protocol = manager1_protocol;
            let machine_addr = machine_addr.clone();
            async move {
                let (pin, tx) = manager1_protocol.claim_machine(machine_addr).await.unwrap();
                tx.send(true).unwrap();
                (pin, manager1_protocol)
            }
        });

        let manager2_task = tokio::spawn({
            let manager2_protocol = manager2_protocol;
            let machine_addr = machine_addr.clone();
            async move {
                let (pin, tx) = manager2_protocol.claim_machine(machine_addr).await.unwrap();
                tx.send(true).unwrap();
                (pin, manager2_protocol)
            }
        });

        let manager3_task = tokio::spawn({
            let manager3_protocol = manager3_protocol;
            let machine_addr = machine_addr;
            async move {
                let (pin, tx) = manager3_protocol.claim_machine(machine_addr).await.unwrap();
                tx.send(true).unwrap();
                (pin, manager3_protocol)
            }
        });

        // Machine should respond to exactly one claim request
        let machine_task = tokio::spawn(async move {
            let (pin, tx) = machine_protocol
                .await_next_incoming_claim_request()
                .await
                .unwrap();
            tx.send(true).unwrap();
            (pin, machine_protocol)
        });

        // Wait for all tasks to complete
        let (
            (pin1, manager1_protocol),
            (pin2, manager2_protocol),
            (pin3, manager3_protocol),
            (pin_machine, machine_protocol),
        ) = tokio::try_join!(manager1_task, manager2_task, manager3_task, machine_task)?;

        // Collect all manager PINs
        let manager_pins = vec![pin1, pin2, pin3];

        // Machine PIN should match one of the manager PINs
        assert!(
            manager_pins.contains(&pin_machine),
            "Machine PIN {} should match one of the manager PINs: {:?}",
            pin_machine,
            manager_pins
        );

        // Wait for claim to complete
        tokio::time::sleep(IROH_WAIT_DELAY * 10).await;

        // Exactly one manager should have successfully claimed the machine
        let machine_count_by_manager = [
            manager1_protocol.list_machines()?.len(),
            manager2_protocol.list_machines()?.len(),
            manager3_protocol.list_machines()?.len(),
        ];

        let total_claims = machine_count_by_manager.iter().sum::<usize>();
        assert_eq!(
            total_claims, 1,
            "Exactly one manager should have claimed the machine"
        );

        // Find which manager succeeded and verify machine state.
        let winning_manager = if machine_count_by_manager[0] == 1 {
            &manager1_protocol
        } else if machine_count_by_manager[1] == 1 {
            &manager2_protocol
        } else {
            &manager3_protocol
        };

        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(winning_manager.get_public_key().await?),
            "Machine should be claimed by the winning manager"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_manager_restart_persistence() -> anyhow::Result<()> {
        let (_machine_protocol, manager_protocol, _machine_temp, manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let machine_id = manager_protocol.list_machines()?[0].0;

        // Set a machine config
        let pk = get_test_public_key()?;
        let federation_invite_code = InviteCode::from_str(
            "fed11qgqpcxnhwden5te0vejkg6tdd9h8gepwd4cxcuewvdshx6p0qvqjpypneenvnkhq0actdl9e4l72ah5gel78dylu5wkc9d3kyy52f62asrl562"
        ).unwrap();

        let machine_config = MachineConfig {
            federation_invite_code,
            claimer_pk: pk,
        };

        manager_protocol
            .set_machine_config(&machine_id, &machine_config)
            .await?;

        // Shutdown and restart manager
        manager_protocol.shutdown().await?;
        drop(manager_protocol);

        let manager_protocol = ManagerProtocol::new(manager_temp.path()).await?;

        // Should still have the machine
        let machines = manager_protocol.list_machines()?;
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].0, machine_id);

        // Should still have the machine config
        let retrieved_config = manager_protocol.get_machine_config(&machine_id).await?;
        assert_eq!(retrieved_config, Some(machine_config));

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_persistence_across_restarts() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let manager_storage_path = tempfile::tempdir()?;

        // Create machine and manager, perform claim
        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        let machine_addr = machine_protocol.node_addr().await?;
        let original_node_id = machine_addr.node_id;

        let (_pin, machine_protocol, manager_protocol) =
            perform_claim(machine_protocol, manager_protocol, true, true).await?;

        // Wait for claim to complete
        wait_for_condition(
            || async { manager_protocol.list_machines().unwrap().len() == 1 },
            10,
        )
        .await?;

        // Shutdown machine
        machine_protocol.shutdown().await?;
        drop(machine_protocol);

        // Restart machine
        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;

        // Should have same node ID (tests key persistence)
        assert_eq!(
            machine_protocol.node_addr().await?.node_id,
            original_node_id
        );

        // Should still be claimed by the same manager (tests claim state persistence)
        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(manager_protocol.get_public_key().await?)
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_doc_key_validation() -> anyhow::Result<()> {
        use super::shared::SharedProtocol;

        let pk = get_test_public_key()?;
        let (federation_id, claimable_contract) = create_test_claimable_contract(pk);

        // Test valid key creation and parsing
        let key = SharedProtocol::create_claimable_contract_machine_doc_key(
            &federation_id,
            &claimable_contract,
        );

        // Key should have correct length (2 prefix + 32 federation + 32 contract = 66)
        assert_eq!(key.len(), 66);

        // Key should start with correct prefix
        assert_eq!(&key[0..2], &super::shared::CLAIMABLE_CONTRACT_PREFIX);

        // Should be able to parse it back
        let (parsed_fed_id, parsed_contract_id) =
            SharedProtocol::parse_incoming_contract_machine_doc_key(&key)?;

        assert_eq!(parsed_fed_id, federation_id);
        assert_eq!(
            parsed_contract_id,
            claimable_contract.contract.contract_id()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_shutdown_idempotency() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let manager_storage_path = tempfile::tempdir()?;

        let machine_protocol = MachineProtocol::new(machine_storage_path.path()).await?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

        // Multiple shutdowns should be safe
        assert!(!machine_protocol.is_shutdown());
        assert!(!manager_protocol.is_shutdown());

        machine_protocol.shutdown().await?;
        manager_protocol.shutdown().await?;

        assert!(machine_protocol.is_shutdown());
        assert!(manager_protocol.is_shutdown());

        // Second shutdown should be idempotent
        machine_protocol.shutdown().await?;
        manager_protocol.shutdown().await?;

        assert!(machine_protocol.is_shutdown());
        assert!(manager_protocol.is_shutdown());

        // Third shutdown should still be safe
        machine_protocol.shutdown().await?;
        manager_protocol.shutdown().await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_config_edge_cases() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let machine_id = manager_protocol.list_machines()?[0].0;

        // Initially no config
        assert_eq!(machine_protocol.get_machine_config().await?, None);
        assert_eq!(
            manager_protocol.get_machine_config(&machine_id).await?,
            None
        );

        // Set a config
        let pk = get_test_public_key()?;
        let federation_invite_code = InviteCode::from_str(
            "fed11qgqpcxnhwden5te0vejkg6tdd9h8gepwd4cxcuewvdshx6p0qvqjpypneenvnkhq0actdl9e4l72ah5gel78dylu5wkc9d3kyy52f62asrl562"
        ).unwrap();

        let machine_config = MachineConfig {
            federation_invite_code: federation_invite_code.clone(),
            claimer_pk: pk,
        };

        manager_protocol
            .set_machine_config(&machine_id, &machine_config)
            .await?;

        // Wait for sync
        wait_for_condition(
            || async { matches!(machine_protocol.get_machine_config().await, Ok(Some(_))) },
            10,
        )
        .await?;

        // Both should see the config
        assert_eq!(
            machine_protocol.get_machine_config().await?,
            Some(machine_config.clone())
        );
        assert_eq!(
            manager_protocol.get_machine_config(&machine_id).await?,
            Some(machine_config.clone())
        );

        // Update the config with different claimer key
        let new_pk = fedimint_core::secp256k1::PublicKey::from_str(
            "02e7798ad2ded4e6dbc6a5a6a891dcb577dadf96842fe500ac46ed5f623aa9042b",
        )
        .unwrap();

        let updated_config = MachineConfig {
            federation_invite_code,
            claimer_pk: new_pk,
        };

        manager_protocol
            .set_machine_config(&machine_id, &updated_config)
            .await?;

        // Wait for sync
        wait_for_condition(
            || async {
                matches!(machine_protocol.get_machine_config().await, Ok(Some(ref config)) if config.claimer_pk == new_pk)
            },
            10,
        )
        .await?;

        // Both should see the updated config
        assert_eq!(
            machine_protocol.get_machine_config().await?,
            Some(updated_config.clone())
        );
        assert_eq!(
            manager_protocol.get_machine_config(&machine_id).await?,
            Some(updated_config)
        );

        Ok(())
    }
}
