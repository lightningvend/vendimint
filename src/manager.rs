use std::path::PathBuf;

use iroh_docs::{Capability, DocTicket};

use crate::shared::SharedProtocol;

const MACHINE_DOC_TICKETS_SUBDIR: &str = "machine_doc_tickets";

pub struct ManagerProtocol {
    shared_protocol: SharedProtocol,
}

impl ManagerProtocol {
    pub async fn new(storage_path: PathBuf) -> anyhow::Result<Self> {
        let manager_protocol = Self {
            shared_protocol: SharedProtocol::new(storage_path).await?,
        };

        // Ensure the machine doc tickets directory exists.
        let machine_doc_tickets_path = manager_protocol.get_machine_doc_ticket_path();
        std::fs::create_dir_all(&machine_doc_tickets_path).unwrap();

        Ok(manager_protocol)
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.shared_protocol.shutdown().await
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.shared_protocol.is_shutdown()
    }

    pub async fn add_machine(&self, machine_doc_ticket: DocTicket) -> anyhow::Result<()> {
        let Capability::Write(namespace_secret) = machine_doc_ticket.capability.clone() else {
            return Err(anyhow::anyhow!(
                "Machine doc ticket must have write capability"
            ));
        };

        let machine_doc_ticket_str = serde_json::to_string(&machine_doc_ticket).unwrap();

        self.shared_protocol
            .get_docs()
            .client()
            .import(machine_doc_ticket)
            .await?;

        std::fs::write(
            self.get_machine_doc_ticket_path()
                .join(namespace_secret.to_string()),
            machine_doc_ticket_str,
        )
        .unwrap();

        Ok(())
    }

    fn get_machine_doc_ticket_path(&self) -> PathBuf {
        self.shared_protocol
            .get_app_storage_path()
            .join(MACHINE_DOC_TICKETS_SUBDIR)
    }
}
