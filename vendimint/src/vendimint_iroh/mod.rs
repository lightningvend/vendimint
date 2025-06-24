mod machine;
mod manager;
mod shared;

pub use machine::MachineProtocol;
pub use manager::ManagerProtocol;
pub use shared::MachineConfig;

#[cfg(test)]
mod tests {
    use crate::vendimint_iroh::shared::KV_PREFIX;

    use super::*;

    use anyhow::Context;
    use futures;
    use std::{str::FromStr, sync::Arc, time::Duration};

    use fedimint_core::{
        Amount, OutPoint, TransactionId, config::FederationId, invite_code::InviteCode,
    };
    use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
    use fedimint_lnv2_remote_client::ClaimableContract;
    use tokio::time::Instant;
    use tpe::{AggregatePublicKey, G1Affine};

    const IROH_WAIT_DELAY: Duration = Duration::from_millis(100);
    const DEFAULT_WAIT_ITERATIONS: u32 = 10;
    const TEST_FEDERATION_INVITE_CODE: &str = "fed11qgqpcxnhwden5te0vejkg6tdd9h8gepwd4cxcuewvdshx6p0qvqjpypneenvnkhq0actdl9e4l72ah5gel78dylu5wkc9d3kyy52f62asrl562";

    /// Helper to create a machine and manager protocol pair with temp directories
    ///
    /// NOTE: The `TempDir` objects MUST be returned and kept alive by the caller!
    /// The protocols only store `PathBuf` internally, not the `TempDir` itself, so if
    /// we don't return the `TempDir` objects, they get dropped at the end of this
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
        Err(anyhow::anyhow!(
            "Condition timeout after {} iterations ({:?} total)",
            timeout_iterations,
            IROH_WAIT_DELAY * timeout_iterations
        ))
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

        // Wait for claim to finish.
        wait_for_machines_listed(&manager_protocol, 1)
            .await
            .context("Failed to complete machine claiming process")?;

        Ok((
            machine_protocol,
            manager_protocol,
            machine_temp,
            manager_temp,
        ))
    }

    /// Helper to create test claimable contract
    fn create_test_federation_id_and_claimable_contract() -> (FederationId, ClaimableContract) {
        let pk = get_test_public_key();
        let federation_id = FederationId::dummy();

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
    fn get_test_public_key() -> fedimint_core::secp256k1::PublicKey {
        fedimint_core::secp256k1::PublicKey::from_str(
            "03e7798ad2ded4e6dbc6a5a6a891dcb577dadf96842fe500ac46ed5f623aa9042b",
        )
        .unwrap()
    }

    /// Helper to create a dummy `NodeId` for testing
    fn create_dummy_node_id() -> iroh::NodeId {
        // Use a dummy string that represents a valid NodeId
        let dummy_hex = "0101010101010101010101010101010101010101010101010101010101010101";
        iroh::NodeId::from_str(dummy_hex).unwrap()
    }

    /// Helper to create standard test machine config
    fn create_test_machine_config() -> MachineConfig {
        MachineConfig {
            federation_invite_code: InviteCode::from_str(TEST_FEDERATION_INVITE_CODE).unwrap(),
            claimer_pk: get_test_public_key(),
        }
    }

    /// Helper to wait for contract synchronization with specific count
    async fn wait_for_contract_sync(
        manager_protocol: &ManagerProtocol,
        expected_count: usize,
    ) -> anyhow::Result<()> {
        wait_for_condition(
            || async {
                manager_protocol
                    .get_claimable_contracts()
                    .await
                    .map(|contracts| contracts.len() == expected_count)
                    .unwrap_or(false)
            },
            DEFAULT_WAIT_ITERATIONS,
        )
        .await
        .with_context(|| format!("Failed waiting for {expected_count} contracts to sync"))
    }

    /// Helper to wait for manager to have claimed machines
    async fn wait_for_machines_listed(
        manager_protocol: &ManagerProtocol,
        expected_count: usize,
    ) -> anyhow::Result<()> {
        wait_for_condition(
            || async { manager_protocol.list_machines().await.unwrap().len() == expected_count },
            DEFAULT_WAIT_ITERATIONS,
        )
        .await
        .with_context(|| format!("Failed waiting for manager to list {expected_count} machines"))
    }

    /// Helper to assert that a given machine and manager both view each other as paired.
    async fn assert_machine_claimed_by_manager(
        machine_protocol: &MachineProtocol,
        manager_protocol: &ManagerProtocol,
    ) -> anyhow::Result<()> {
        // Assert the machine sees the manager as its manager.
        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(manager_protocol.get_public_key().await?),
            "Machine should be claimed by the specified manager"
        );

        // Assert the manager sees the machine as its machine.
        let machine_node_id = machine_protocol.node_addr().await.unwrap().node_id;
        assert!(
            manager_protocol
                .list_machines()
                .await?
                .into_iter()
                .any(|(pubkey, _)| pubkey == machine_node_id),
            "Manager should own the machine"
        );
        Ok(())
    }

    /// Helper to set and verify machine config with proper waiting
    async fn set_and_verify_machine_config(
        machine_protocol: &MachineProtocol,
        manager_protocol: &ManagerProtocol,
        machine_id: &iroh::NodeId,
        config: &MachineConfig,
    ) -> anyhow::Result<()> {
        manager_protocol
            .set_machine_config(machine_id, config)
            .await?;

        // Wait for machine config to be set on the machine.
        wait_for_condition(
            || async { matches!(machine_protocol.get_machine_config().await, Ok(Some(_))) },
            DEFAULT_WAIT_ITERATIONS,
        )
        .await
        .context("Failed waiting for machine config to be set")?;

        assert_eq!(
            machine_protocol.get_machine_config().await?,
            Some(config.clone())
        );
        assert_eq!(
            manager_protocol.get_machine_config(machine_id).await?,
            Some(config.clone())
        );

        Ok(())
    }

    /// Helper to write contract and wait for sync
    async fn write_and_sync_contract(
        machine_protocol: &MachineProtocol,
        manager_protocol: &ManagerProtocol,
        federation_id: &FederationId,
        contract: &ClaimableContract,
    ) -> anyhow::Result<()> {
        let contract_count_before = manager_protocol.get_claimable_contracts().await?.len();

        machine_protocol
            .write_payment_to_machine_doc(federation_id, contract)
            .await?;

        wait_for_contract_sync(manager_protocol, contract_count_before + 1).await
    }

    /// Helper to spawn a manager claim task
    fn spawn_manager_claim_task(
        manager_protocol: ManagerProtocol,
        machine_addr: iroh::NodeAddr,
    ) -> tokio::task::JoinHandle<(u32, ManagerProtocol)> {
        tokio::spawn(async move {
            let (pin, tx) = manager_protocol.claim_machine(machine_addr).await.unwrap();
            tx.send(true).unwrap();
            (pin, manager_protocol)
        })
    }

    #[tokio::test]
    async fn test_basic_claim_flow() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        assert_machine_claimed_by_manager(&machine_protocol, &manager_protocol).await?;
        assert_eq!(manager_protocol.list_machines().await.unwrap().len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_machine_config_operations() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        assert_eq!(machine_protocol.get_machine_config().await?, None);

        let machine_config = create_test_machine_config();
        let machine_id = manager_protocol.list_machines().await.unwrap()[0].0;

        set_and_verify_machine_config(
            &machine_protocol,
            &manager_protocol,
            &machine_id,
            &machine_config,
        )
        .await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_claimable_contract_operations() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let (federation_id, claimable_contract) =
            create_test_federation_id_and_claimable_contract();

        write_and_sync_contract(
            &machine_protocol,
            &manager_protocol,
            &federation_id,
            &claimable_contract,
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

        wait_for_machines_listed(&manager1_protocol, 1).await?;
        assert_machine_claimed_by_manager(&machine_protocol, &manager1_protocol).await?;

        // Second manager attempts to claim
        let machine_addr2 = machine_protocol.node_addr().await?;
        let (_pin_mgr2, tx_mgr2) = manager2_protocol.claim_machine(machine_addr2).await?;
        tx_mgr2.send(true).unwrap();

        tokio::time::sleep(IROH_WAIT_DELAY).await;

        assert_eq!(
            machine_protocol.get_manager_pubkey().await,
            Some(manager1_protocol.get_public_key().await?)
        );
        assert_eq!(manager2_protocol.list_machines().await?.len(), 0);

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
        assert_eq!(manager_protocol.list_machines().await?.len(), 0);

        // After rejection, another claim should be possible.
        let (_pin, machine_protocol, manager_protocol) =
            perform_claim(machine_protocol, manager_protocol, true, true).await?;

        tokio::time::sleep(IROH_WAIT_DELAY).await;

        assert_machine_claimed_by_manager(&machine_protocol, &manager_protocol).await?;
        assert_eq!(manager_protocol.list_machines().await.unwrap().len(), 1);

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
        assert_eq!(manager1_protocol.list_machines().await?.len(), 0);

        // Second manager should be able to claim, since the first claim was aborted
        let (_pin, machine_protocol, manager2_protocol) =
            perform_claim(machine_protocol, manager2_protocol, true, true).await?;

        tokio::time::sleep(IROH_WAIT_DELAY).await;

        assert_machine_claimed_by_manager(&machine_protocol, &manager2_protocol).await?;
        assert_eq!(manager2_protocol.list_machines().await.unwrap().len(), 1);

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

        wait_for_machines_listed(&manager_protocol, 1).await?;
        assert_machine_claimed_by_manager(&machine_protocol, &manager_protocol).await?;

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

        let machines = manager_protocol.list_machines().await?;
        assert!(machines.is_empty(), "New manager should have no machines");

        Ok(())
    }

    #[tokio::test]
    async fn test_manager_get_machine_config_nonexistent() -> anyhow::Result<()> {
        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;

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

        let machine_config = create_test_machine_config();
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

        let (federation_id, claimable_contract) =
            create_test_federation_id_and_claimable_contract();

        // Write the same contract twice (this should be idempotent)
        machine_protocol
            .write_payment_to_machine_doc(&federation_id, &claimable_contract)
            .await?;
        machine_protocol
            .write_payment_to_machine_doc(&federation_id, &claimable_contract)
            .await?;

        wait_for_contract_sync(&manager_protocol, 1).await?;

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

        let (federation_id, claimable_contract) =
            create_test_federation_id_and_claimable_contract();

        let contracts = manager_protocol.get_claimable_contracts().await?;
        assert_eq!(contracts.len(), 0);

        write_and_sync_contract(
            &machine_protocol,
            &manager_protocol,
            &federation_id,
            &claimable_contract,
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

        let (federation_id, claimable_contract) =
            create_test_federation_id_and_claimable_contract();

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
        let machine_config = create_test_machine_config();

        // Test serialization and deserialization
        let serialized = serde_json::to_string(&machine_config)?;
        let deserialized: MachineConfig = serde_json::from_str(&serialized)?;

        assert_eq!(machine_config, deserialized);
        assert_eq!(
            machine_config.federation_invite_code.to_string(),
            TEST_FEDERATION_INVITE_CODE
        );
        assert_eq!(machine_config.claimer_pk, get_test_public_key());

        Ok(())
    }

    /// Test concurrent claim attempts with configurable number of managers
    async fn test_concurrent_claim_attempts_with_managers(
        num_managers: usize,
    ) -> anyhow::Result<()> {
        let machine_temp = tempfile::tempdir()?;
        let machine_protocol = MachineProtocol::new(machine_temp.path()).await?;

        let mut manager_protocols = Vec::new();

        // Ignore clippy warning, this is to keep temp dirs alive.
        // It serves a purpose even though it's not used directly.
        #[allow(clippy::collection_is_never_read)]
        let mut temp_dirs = Vec::new();

        // Create managers.
        for _ in 0..num_managers {
            let manager_storage_path = tempfile::tempdir()?;
            let manager_protocol = ManagerProtocol::new(manager_storage_path.path()).await?;
            manager_protocols.push(manager_protocol);
            temp_dirs.push(manager_storage_path); // Keep temp dir alive
        }

        let machine_addr = machine_protocol.node_addr().await?;

        let machine_protocol_arc = Arc::new(machine_protocol);

        // Machine should respond to exactly one claim request
        let machine_protocol_arc_clone = machine_protocol_arc.clone();
        let (machine_pin_sender, mut machine_pin_receiver) =
            tokio::sync::mpsc::channel(num_managers);
        let machine_task = tokio::spawn(async move {
            loop {
                let (pin, tx) = machine_protocol_arc_clone
                    .await_next_incoming_claim_request()
                    .await
                    .unwrap();
                tx.send(true).unwrap();
                machine_pin_sender.send(pin).await.unwrap();
            }
        });

        // Start concurrent claim attempts for all managers
        let mut manager_tasks = Vec::new();
        for manager_protocol in manager_protocols {
            let task = spawn_manager_claim_task(manager_protocol, machine_addr.clone());
            manager_tasks.push(task);
        }

        // Wait for all tasks to complete
        let mut manager_results = Vec::new();
        for task in manager_tasks {
            manager_results.push(task.await?);
        }

        tokio::time::sleep(IROH_WAIT_DELAY * 10).await;

        let mut machine_pins = Vec::new();
        machine_pin_receiver
            .recv_many(&mut machine_pins, machine_pin_receiver.len())
            .await;

        // Machine should only be able to accept one claim request.
        assert_eq!(machine_pins.len(), 1, "Machine should have exactly one PIN");
        let pin_machine = machine_pins[0];

        // Collect all manager PINs and verify machine PIN matches one of them
        let manager_pins: Vec<u32> = manager_results.iter().map(|(pin, _)| *pin).collect();
        assert!(
            manager_pins.contains(&pin_machine),
            "Machine PIN {pin_machine} should match one of the manager PINs: {manager_pins:?}"
        );

        // Wait for claim to complete
        tokio::time::sleep(IROH_WAIT_DELAY * 10).await;

        // Exactly one manager should have successfully claimed the machine
        let machine_count_futures: Vec<_> = manager_results
            .iter()
            .map(|(_, protocol)| protocol.list_machines())
            .collect();
        let machine_lists = futures::future::try_join_all(machine_count_futures).await?;
        let machine_counts: Vec<usize> = machine_lists.iter().map(|list| list.len()).collect();

        let total_claims = machine_counts.iter().sum::<usize>();
        assert_eq!(
            total_claims, 1,
            "Exactly one manager should have claimed the machine"
        );

        // Find which manager succeeded and verify machine state
        let winning_manager_index = machine_counts
            .iter()
            .position(|&count| count == 1)
            .expect("Should have exactly one winning manager");
        let winning_manager = &manager_results[winning_manager_index].1;

        assert_eq!(
            machine_protocol_arc.get_manager_pubkey().await,
            Some(winning_manager.get_public_key().await?),
            "Machine should be claimed by the winning manager"
        );

        machine_task.abort();

        Ok(())
    }

    #[tokio::test]
    async fn test_concurrent_claim_attempts_single_manager() -> anyhow::Result<()> {
        test_concurrent_claim_attempts_with_managers(1).await
    }

    #[tokio::test]
    async fn test_concurrent_claim_attempts_two_managers() -> anyhow::Result<()> {
        test_concurrent_claim_attempts_with_managers(2).await
    }

    #[tokio::test]
    async fn test_concurrent_claim_attempts_five_managers() -> anyhow::Result<()> {
        test_concurrent_claim_attempts_with_managers(5).await
    }

    #[tokio::test]
    async fn test_concurrent_claim_attempts_twenty_managers() -> anyhow::Result<()> {
        test_concurrent_claim_attempts_with_managers(20).await
    }

    #[tokio::test]
    async fn test_manager_restart_persistence() -> anyhow::Result<()> {
        let (_machine_protocol, manager_protocol, _machine_temp, manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let machine_id = manager_protocol.list_machines().await?[0].0;

        // Set a machine config
        let machine_config = create_test_machine_config();

        manager_protocol
            .set_machine_config(&machine_id, &machine_config)
            .await?;

        // Shutdown and restart manager
        manager_protocol.shutdown().await?;
        drop(manager_protocol);

        let manager_protocol = ManagerProtocol::new(manager_temp.path()).await?;

        // Should still have the machine
        let machines = manager_protocol.list_machines().await?;
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

        wait_for_machines_listed(&manager_protocol, 1).await?;

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

        let (federation_id, claimable_contract) =
            create_test_federation_id_and_claimable_contract();

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

        let machine_id = manager_protocol.list_machines().await?[0].0;

        // Initially no config
        assert_eq!(machine_protocol.get_machine_config().await?, None);
        assert_eq!(
            manager_protocol.get_machine_config(&machine_id).await?,
            None
        );

        // Set a config
        let machine_config = create_test_machine_config();

        set_and_verify_machine_config(
            &machine_protocol,
            &manager_protocol,
            &machine_id,
            &machine_config,
        )
        .await?;

        // Update the config with different claimer key
        let new_pk = fedimint_core::secp256k1::PublicKey::from_str(
            "02e7798ad2ded4e6dbc6a5a6a891dcb577dadf96842fe500ac46ed5f623aa9042b",
        )
        .unwrap();

        let updated_config = MachineConfig {
            federation_invite_code: InviteCode::from_str(TEST_FEDERATION_INVITE_CODE)?,
            claimer_pk: new_pk,
        };

        set_and_verify_machine_config(
            &machine_protocol,
            &manager_protocol,
            &machine_id,
            &updated_config,
        )
        .await?;

        Ok(())
    }

    #[tokio::test]
    async fn test_kv_api_basic_operations() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let machine_id = manager_protocol.list_machines().await?[0].0;

        // Test setting and getting a key/value pair
        let key = b"test_key";
        let value = b"test_value";

        // Set value using machine protocol
        machine_protocol.set_kv_value(key, value).await?;

        // Get value using machine protocol
        let entry = machine_protocol.get_kv_value(key).await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();

        // Verify entry metadata
        assert_eq!(entry.content_len(), value.len() as u64);
        assert_eq!(&entry.key()[KV_PREFIX.len()..], key);

        // Get value using manager protocol
        let entry = manager_protocol.get_kv_value(&machine_id, key).await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();

        // Verify same metadata
        assert_eq!(entry.content_len(), value.len() as u64);
        assert_eq!(&entry.key()[KV_PREFIX.len()..], key);

        // Test non-existent key
        let entry = machine_protocol.get_kv_value(b"nonexistent").await?;
        assert!(entry.is_none());

        let entry = manager_protocol
            .get_kv_value(&machine_id, b"nonexistent")
            .await?;
        assert!(entry.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_kv_empty_by_default() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let machine_id = manager_protocol.list_machines().await?[0].0;

        // KV store should be empty for new machines.
        assert!(
            machine_protocol
                .get_kv_value(b"nonexistent")
                .await?
                .is_none()
        );
        assert!(
            manager_protocol
                .get_kv_value(&machine_id, b"nonexistent")
                .await?
                .is_none()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_kv_api_multiple_entries() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let machine_id = manager_protocol.list_machines().await?[0].0;

        // Set multiple key/value pairs
        let test_data = vec![
            (b"key1".as_slice(), b"value1".as_slice()),
            (b"key2".as_slice(), b"value2".as_slice()),
            (b"key3".as_slice(), b"value3".as_slice()),
        ];

        for (key, value) in &test_data {
            machine_protocol.set_kv_value(key, value).await?;
        }

        // Brief wait to ensure all values are stored
        tokio::time::sleep(IROH_WAIT_DELAY).await;

        // Get all entries using machine protocol
        let entries = machine_protocol.get_kv_entries().await?;
        assert_eq!(entries.len(), test_data.len());

        // Verify each entry metadata
        for entry in &entries {
            // Extract the user key (remove KV_PREFIX)
            let user_key = &entry.key()[KV_PREFIX.len()..];

            // Find the corresponding test data
            let expected_value = test_data
                .iter()
                .find(|(key, _)| *key == user_key)
                .map(|(_, value)| *value)
                .expect("Entry key should match test data");

            assert_eq!(entry.content_len(), expected_value.len() as u64);
        }

        // Get all entries using manager protocol
        let entries = manager_protocol.get_kv_entries(&machine_id).await?;
        assert_eq!(entries.len(), test_data.len());

        // Verify we can retrieve individual entries via manager
        for (key, value) in &test_data {
            let entry = manager_protocol.get_kv_value(&machine_id, key).await?;
            assert!(entry.is_some());
            assert_eq!(entry.unwrap().content_len(), value.len() as u64);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_kv_api_update_existing_key() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let machine_id = manager_protocol.list_machines().await?[0].0;

        let key = b"update_key";
        let initial_value = b"initial_value";
        let updated_value = b"new_updated_value_longer";

        // Set initial value
        machine_protocol.set_kv_value(key, initial_value).await?;

        // Verify initial value
        let entry = machine_protocol.get_kv_value(key).await?;
        assert!(entry.is_some());
        let initial_entry = entry.unwrap();
        assert_eq!(initial_entry.content_len(), initial_value.len() as u64);
        let initial_hash = initial_entry.content_hash();

        // Wait for change to sync to manager.
        wait_for_condition(
            || async {
                manager_protocol
                    .get_kv_value(&machine_id, key)
                    .await
                    .unwrap()
                    .is_some()
            },
            10,
        )
        .await?;

        // Update value using manager protocol
        manager_protocol
            .set_kv_value(&machine_id, key, updated_value)
            .await?;

        // Verify value was updated via manager protocol (should see new content immediately)
        let entry = manager_protocol.get_kv_value(&machine_id, key).await?;
        assert!(entry.is_some());
        let updated_entry = entry.unwrap();
        assert_eq!(updated_entry.content_len(), updated_value.len() as u64);
        let updated_hash = updated_entry.content_hash();

        // Content hash should be different (verifies update actually happened)
        assert_ne!(initial_hash, updated_hash);

        // Wait for update to sync to machine protocol
        wait_for_condition(
            || async {
                if let Ok(Some(entry)) = machine_protocol.get_kv_value(key).await {
                    entry.content_len() == updated_value.len() as u64
                } else {
                    false
                }
            },
            DEFAULT_WAIT_ITERATIONS,
        )
        .await?;

        // Verify update synced to machine protocol
        let entry = machine_protocol.get_kv_value(key).await?;
        assert!(entry.is_some());
        let synced_entry = entry.unwrap();
        assert_eq!(synced_entry.content_len(), updated_value.len() as u64);
        assert_eq!(synced_entry.content_hash(), updated_hash);

        Ok(())
    }

    #[tokio::test]
    async fn test_kv_api_namespace_isolation() -> anyhow::Result<()> {
        let (machine_protocol, manager_protocol, _machine_temp, _manager_temp) =
            create_claimed_machine_manager_pair().await?;

        let machine_id = manager_protocol.list_machines().await?[0].0;

        // Set a KV entry
        machine_protocol
            .set_kv_value(b"test_key", b"test_value")
            .await?;

        // Set a machine config (different namespace)
        let machine_config = create_test_machine_config();
        manager_protocol
            .set_machine_config(&machine_id, &machine_config)
            .await?;

        // Write a claimable contract (different namespace)
        let (federation_id, claimable_contract) =
            create_test_federation_id_and_claimable_contract();
        machine_protocol
            .write_payment_to_machine_doc(&federation_id, &claimable_contract)
            .await?;

        // KV entries should only return KV data, not config or contracts
        let kv_entries = machine_protocol.get_kv_entries().await?;
        assert_eq!(kv_entries.len(), 1);

        // Verify the KV entry is correct
        let entry = &kv_entries[0];
        assert_eq!(entry.content_len(), b"test_value".len() as u64);

        // Verify the user key (without prefix)
        let user_key = &entry.key()[KV_PREFIX.len()..];
        assert_eq!(user_key, b"test_key");

        // Verify we can still access the machine config and contracts via their APIs
        let config = machine_protocol.get_machine_config().await?;
        assert!(config.is_some());

        let contracts = manager_protocol.get_claimable_contracts().await?;
        assert_eq!(contracts.len(), 1);

        // KV operations should not interfere with other namespaces
        let kv_entry = machine_protocol.get_kv_value(b"test_key").await?;
        assert!(kv_entry.is_some());

        Ok(())
    }
}
