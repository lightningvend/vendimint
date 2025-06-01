use std::path::{Path, PathBuf};
use tokio::sync::oneshot;

use fedimint_core::db::DatabaseValue;
use iroh::{NodeAddr, protocol::Router};
use iroh_docs::{Capability, DocTicket, protocol::Docs};
use uuid::Uuid;

use shared::{
    CLAIM_ALPN, FEDERATION_INVITE_CODE_KEY, MachineConfig, SharedProtocol,
    claim_pin_from_keying_material,
};

const MACHINE_DOC_TICKETS_SUBDIR: &str = "machine_doc_tickets";

pub struct ManagerProtocol {
    router: Router,
    docs: Docs<iroh_blobs::store::fs::Store>,
    app_storage_path: PathBuf,
}

impl ManagerProtocol {
    pub async fn new(storage_path: &Path) -> anyhow::Result<Self> {
        let shared_protocol = SharedProtocol::new(storage_path).await?;

        let manager_protocol = Self {
            router: shared_protocol.router_builder.spawn().await?,
            docs: shared_protocol.docs,
            app_storage_path: shared_protocol.app_storage_path,
        };

        // Ensure the machine doc tickets directory exists.
        let machine_doc_tickets_path = manager_protocol.get_machine_doc_ticket_path();
        std::fs::create_dir_all(&machine_doc_tickets_path).unwrap();

        Ok(manager_protocol)
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.router.shutdown().await
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
                FEDERATION_INVITE_CODE_KEY.to_bytes(),
                serde_json::to_vec(machine_config).unwrap(),
            )
            .await?;
        Ok(())
    }

    fn get_machine_doc_ticket_path(&self) -> PathBuf {
        self.app_storage_path.join(MACHINE_DOC_TICKETS_SUBDIR)
    }
}
