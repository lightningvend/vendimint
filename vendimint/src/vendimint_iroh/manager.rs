use fedimint_core::{config::FederationId, db::DatabaseValue};
use fedimint_lnv2_remote_client::ClaimableContract;
use futures_util::StreamExt;

use iroh::{
    NodeAddr,
    protocol::{ProtocolHandler, Router},
};
use iroh_blobs::net_protocol::Blobs;
use iroh_docs::{
    Capability, DocTicket,
    protocol::Docs,
    store::{QueryBuilder, SingleLatestPerKeyQuery},
};
use std::path::{Path, PathBuf};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::shared::{
    CLAIM_ALPN, CLAIMABLE_CONTRACT_PREFIX, MACHINE_CONFIG_KEY, MachineConfig, SharedProtocol,
    claim_pin_from_keying_material,
};

const MACHINE_DOC_TICKETS_SUBDIR: &str = "machine_doc_tickets";

pub struct ManagerProtocol {
    router: Router,
    blobs: Blobs<iroh_blobs::store::fs::Store>,
    docs: Docs<iroh_blobs::store::fs::Store>,
    app_storage_path: PathBuf,
}

impl ManagerProtocol {
    pub async fn new(storage_path: &Path) -> anyhow::Result<Self> {
        let shared_protocol = SharedProtocol::new(storage_path).await?;

        let manager_protocol = Self {
            router: shared_protocol.router_builder.spawn().await?,
            blobs: shared_protocol.blobs,
            docs: shared_protocol.docs,
            app_storage_path: shared_protocol.app_storage_path,
        };

        // Ensure the machine doc tickets directory exists.
        let machine_doc_tickets_path = manager_protocol.get_machine_doc_ticket_path();
        std::fs::create_dir_all(&machine_doc_tickets_path).unwrap();

        Ok(manager_protocol)
    }

    pub async fn shutdown(&self) {
        self.router
            .shutdown()
            .await
            .expect("No `ProtocolHandler` should ever panic");
        self.blobs.shutdown().await;
        self.docs.shutdown().await;
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.router.is_shutdown()
    }

    pub async fn claim_machine(
        &self,
        node_addr: NodeAddr,
    ) -> anyhow::Result<(u32, oneshot::Sender<bool>)> {
        let conn = self
            .router
            .endpoint()
            .connect(node_addr, CLAIM_ALPN)
            .await?;

        let pin = claim_pin_from_keying_material(&conn);
        let (tx, rx) = oneshot::channel();
        let docs = self.docs.clone();
        let machine_doc_ticket_path = self.get_machine_doc_ticket_path();
        tokio::spawn(async move {
            if rx.await.unwrap_or(false) {
                if let Ok(mut recv) = conn.accept_uni().await {
                    if let Ok(bytes) = recv.read_to_end(1024 * 1024).await {
                        if let Ok(machine_doc_ticket) = serde_json::from_slice::<DocTicket>(&bytes)
                        {
                            if matches!(machine_doc_ticket.capability, Capability::Write(_)) {
                                let _ = docs.client().import(machine_doc_ticket.clone()).await;
                                let machine_id = Uuid::new_v4();
                                let _ = std::fs::write(
                                    machine_doc_ticket_path.join(machine_id.to_string()),
                                    serde_json::to_string(&machine_doc_ticket).unwrap(),
                                );
                            }
                        }
                    }
                }
            }
            conn.close(0u32.into(), b"done");
        });

        Ok((pin, tx))
    }

    pub fn get_machine(&self, machine_id: &Uuid) -> anyhow::Result<DocTicket> {
        let machine_doc_ticket_path = self
            .get_machine_doc_ticket_path()
            .join(machine_id.to_string());
        let machine_doc_ticket_str = std::fs::read_to_string(&machine_doc_ticket_path)?;
        let machine_doc_ticket: DocTicket = serde_json::from_str(&machine_doc_ticket_str)?;
        Ok(machine_doc_ticket)
    }

    pub fn list_machines(&self) -> anyhow::Result<Vec<(Uuid, DocTicket)>> {
        let machine_doc_tickets_path = self.get_machine_doc_ticket_path();
        let mut machines = Vec::new();

        for entry in std::fs::read_dir(machine_doc_tickets_path)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let file_name = entry.file_name().into_string().unwrap();
                let machine_id = Uuid::parse_str(&file_name).unwrap();
                let machine_doc_ticket_str = std::fs::read_to_string(entry.path())?;
                let machine_doc_ticket: DocTicket =
                    serde_json::from_str(&machine_doc_ticket_str).unwrap();
                machines.push((machine_id, machine_doc_ticket));
            }
        }

        Ok(machines)
    }

    pub async fn get_machine_config(
        &self,
        machine_id: &Uuid,
    ) -> anyhow::Result<Option<MachineConfig>> {
        let machine_doc_ticket = self.get_machine(machine_id)?;
        let machine_doc = self
            .docs
            .client()
            .open(machine_doc_ticket.capability.id())
            .await?
            .unwrap();

        let Some(entry) = machine_doc
            .get_one(
                QueryBuilder::<SingleLatestPerKeyQuery>::default()
                    .key_exact(MACHINE_CONFIG_KEY.to_bytes()),
            )
            .await?
        else {
            return Ok(None);
        };

        let bytes = self
            .blobs
            .client()
            .read_to_bytes(entry.content_hash())
            .await?;

        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    pub async fn set_machine_config(
        &self,
        machine_id: &Uuid,
        machine_config: &MachineConfig,
    ) -> anyhow::Result<()> {
        let machine_doc_ticket = self.get_machine(machine_id)?;
        let machine_doc = self
            .docs
            .client()
            .open(machine_doc_ticket.capability.id())
            .await?
            .unwrap();
        machine_doc
            .set_bytes(
                self.docs.client().authors().default().await?,
                MACHINE_CONFIG_KEY.to_bytes(),
                serde_json::to_vec(machine_config).unwrap(),
            )
            .await?;
        Ok(())
    }

    pub async fn get_claimable_contracts(
        &self,
    ) -> anyhow::Result<Vec<(Uuid, FederationId, ClaimableContract)>> {
        let mut payments = Vec::new();

        for (machine_id, machine_doc_ticket) in self.list_machines()? {
            let machine_doc = self
                .docs
                .client()
                .open(machine_doc_ticket.capability.id())
                .await?
                .unwrap();

            let mut contract_stream = machine_doc
                .get_many(
                    // Note: Queries exclude empty values by default.
                    QueryBuilder::<SingleLatestPerKeyQuery>::default()
                        .key_prefix(CLAIMABLE_CONTRACT_PREFIX)
                        .build(),
                )
                .await?;

            while let Some(entry_or) = contract_stream.next().await {
                let Ok(entry) = entry_or else {
                    continue;
                };

                let Ok((federation_id, _contract_id)) =
                    SharedProtocol::parse_incoming_contract_machine_doc_key(entry.key())
                else {
                    continue;
                };

                let bytes = self
                    .blobs
                    .client()
                    .read_to_bytes(entry.content_hash())
                    .await?;

                // TODO: Add a devimint test to see if this block is necessary.
                if bytes.is_empty() {
                    // This means the manager has previously removed the
                    // contract by setting the value to an empty byte array.
                    continue;
                }

                let claimable_contract: ClaimableContract = serde_json::from_slice(&bytes)?;

                payments.push((machine_id, federation_id, claimable_contract));
            }
        }

        Ok(payments)
    }

    pub async fn remove_claimable_contracts(
        &self,
        // TODO: Change to a `HashMap<Uuid, (FederationId, ClaimableContract)>`
        // so we don't have to re-fetch the machine doc for every contract.
        claimable_contracts: Vec<(Uuid, FederationId, ClaimableContract)>,
    ) -> anyhow::Result<()> {
        let author_id = self.docs.client().authors().default().await?;

        for (machine_id, federation_id, claimable_contract) in claimable_contracts {
            let machine_doc_ticket = self.get_machine(&machine_id)?;
            let machine_doc = self
                .docs
                .client()
                .open(machine_doc_ticket.capability.id())
                .await?
                .unwrap();

            let key = SharedProtocol::create_claimable_contract_machine_doc_key(
                &federation_id,
                &claimable_contract,
            );

            machine_doc.del(author_id, key).await?;
        }

        Ok(())
    }

    fn get_machine_doc_ticket_path(&self) -> PathBuf {
        self.app_storage_path.join(MACHINE_DOC_TICKETS_SUBDIR)
    }
}
