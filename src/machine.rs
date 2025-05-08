use std::path::PathBuf;

use fedimint_lnv2_common::contracts::IncomingContract;
use iroh_docs::{
    DocTicket,
    rpc::{
        AddrInfoOptions,
        client::docs::{Doc, ShareMode},
        proto::RpcService,
    },
};
use quic_rpc::client::FlumeConnector;

use crate::shared::SharedProtocol;

const MACHINE_DOC_TICKET_PATH: &str = "machine_doc_ticket.json";

pub struct MachineProtocol {
    shared_protocol: SharedProtocol,
}

impl MachineProtocol {
    pub async fn new(storage_path: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            shared_protocol: SharedProtocol::new(storage_path).await?,
        })
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.shared_protocol.shutdown().await
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.shared_protocol.is_shutdown()
    }

    pub async fn share_machine_doc(
        &self,
        mode: ShareMode,
        addr_info_options: AddrInfoOptions,
    ) -> anyhow::Result<DocTicket> {
        self.get_or_create_machine_doc()
            .await
            .unwrap()
            .share(mode, addr_info_options)
            .await
    }

    pub async fn write_payment_to_machine_doc(
        &self,
        incoming_contract: IncomingContract,
    ) -> anyhow::Result<()> {
        let doc = self.get_or_create_machine_doc().await?;

        let key = SharedProtocol::create_incoming_contract_machine_doc_key(&incoming_contract);

        doc.set_bytes(
            self.shared_protocol.get_author_id().await?,
            key,
            bincode::serde::encode_to_vec(incoming_contract, bincode::config::standard()).unwrap(),
        )
        .await
        .unwrap();
        Ok(())
    }

    /// Creates an iroh doc (or returns the existing one) for use in storing/transferring data
    /// related to payments received to the current device. Should only be called on a machine.
    async fn get_or_create_machine_doc(&self) -> anyhow::Result<Doc<FlumeConnector<RpcService>>> {
        let doc_ticket_path = self
            .shared_protocol
            .get_app_storage_path()
            .join(MACHINE_DOC_TICKET_PATH);

        let doc_ticket_or: Option<DocTicket> = match std::fs::read_to_string(&doc_ticket_path) {
            Ok(doc_ticket_str) => serde_json::from_str(&doc_ticket_str).ok(),
            Err(_) => None,
        };

        if let Some(doc_ticket) = doc_ticket_or {
            return self
                .shared_protocol
                .get_docs()
                .client()
                .open(doc_ticket.capability.id())
                .await
                .map(|doc_or| doc_or.unwrap());
        }

        let new_doc = self.shared_protocol.get_docs().client().create().await?;

        // Save the doc ticket to a file for later use.
        let new_doc_ticket = new_doc.share(ShareMode::Write, AddrInfoOptions::Id).await?;
        let new_doc_ticket_str = serde_json::to_string(&new_doc_ticket).unwrap();
        std::fs::write(&doc_ticket_path, new_doc_ticket_str).unwrap();

        Ok(new_doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_machine_protocol() -> anyhow::Result<()> {
        let storage_path = tempfile::tempdir().unwrap();
        let machine_protocol = MachineProtocol::new(storage_path.path().to_path_buf()).await?;

        let doc_ticket = machine_protocol
            .share_machine_doc(ShareMode::Write, AddrInfoOptions::Id)
            .await?;

        // Shutdown and restart to test basic persistence.
        machine_protocol.shutdown().await?;
        drop(machine_protocol);

        let machine_protocol = MachineProtocol::new(storage_path.path().to_path_buf()).await?;

        assert_eq!(
            format!(
                "{:?}",
                machine_protocol
                    .share_machine_doc(ShareMode::Write, AddrInfoOptions::Id)
                    .await?
                    .capability
            ),
            format!("{:?}", doc_ticket.capability)
        );

        Ok(())
    }
}
