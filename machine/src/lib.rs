use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use fedimint_core::config::FederationId;
use fedimint_lnv2_common::contracts::IncomingContract;
use iroh::{
    NodeAddr, PublicKey,
    endpoint::Connection,
    protocol::{ProtocolHandler, Router},
};
use iroh_blobs::net_protocol::Blobs;
use iroh_docs::protocol::Docs;
use iroh_docs::{
    DocTicket,
    rpc::{
        AddrInfoOptions,
        client::docs::{Doc, ShareMode},
        proto::RpcService,
    },
    store::Query,
};
use n0_future::boxed::BoxFuture;
use quic_rpc::client::FlumeConnector;
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, mpsc, oneshot},
};

use shared::{
    CLAIM_ALPN, FEDERATION_INVITE_CODE_KEY, MachineConfig, SharedProtocol,
    claim_pin_from_keying_material,
};

const MACHINE_DOC_TICKET_PATH: &str = "machine_doc_ticket.json";
const MACHINE_MANAGER_PUBLIC_KEY_PATH: &str = "machine_manager_public_key.json";

pub struct MachineProtocol {
    router: Router,
    blobs: Blobs<iroh_blobs::store::fs::Store>,
    docs: Docs<iroh_blobs::store::fs::Store>,
    app_storage_path: PathBuf,
    claim_request_receiver: mpsc::Receiver<(u32, oneshot::Sender<bool>)>,
}

#[derive(Clone, Debug)]
struct ClaimHandler {
    docs: Docs<iroh_blobs::store::fs::Store>,
    app_storage_path: PathBuf,
    claim_request_sender: mpsc::Sender<(u32, oneshot::Sender<bool>)>,
    claimed_manager_pubkey: Arc<Mutex<Option<PublicKey>>>,
}

impl ProtocolHandler for ClaimHandler {
    fn accept(&self, connection: Connection) -> BoxFuture<anyhow::Result<()>> {
        let this = self.clone();

        Box::pin(async move {
            let mut claimed_manager_pubkey_lock = this.claimed_manager_pubkey.lock().await;

            let claimer_pubkey = connection.remote_node_id()?;

            if let Some(claimed_manager_pubkey) = claimed_manager_pubkey_lock.as_ref() {
                // Only close if the claimer is not the original claimer.
                // This way, the original claimer can re-call idempotently.
                if claimed_manager_pubkey != &claimer_pubkey {
                    connection.close(0u32.into(), b"already_claimed");
                    return Ok(());
                }
            }

            let pin = claim_pin_from_keying_material(&connection);

            let (tx, rx) = oneshot::channel();

            this.claim_request_sender
                .send((pin, tx))
                .await
                .map_err(|_| anyhow::anyhow!("receiver dropped"))?;

            if rx.await.unwrap_or(false) {
                let doc = get_or_create_machine_doc(&this.app_storage_path, &this.docs).await?;
                let ticket = doc
                    .share(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
                    .await?;

                let manager_public_key_path =
                    this.app_storage_path.join(MACHINE_MANAGER_PUBLIC_KEY_PATH);
                let claimer_pubkey_str = serde_json::to_string(&claimer_pubkey).unwrap();
                std::fs::write(&manager_public_key_path, claimer_pubkey_str).unwrap();

                let mut send = connection.open_uni().await?;
                let ticket_bytes = serde_json::to_vec(&ticket)?;
                send.write_all(&ticket_bytes).await?;
                send.finish()?;
                send.stopped().await?;
                send.shutdown().await?;
                connection.close(0u32.into(), b"finished");

                *claimed_manager_pubkey_lock = Some(claimer_pubkey);
                drop(claimed_manager_pubkey_lock);
            } else {
                connection.close(0u32.into(), b"rejected");
            }

            Ok(())
        })
    }
}

impl MachineProtocol {
    pub async fn new(storage_path: &Path) -> anyhow::Result<Self> {
        let mut shared_protocol = SharedProtocol::new(storage_path).await?;

        let manager_public_key_path = shared_protocol
            .app_storage_path
            .join(MACHINE_MANAGER_PUBLIC_KEY_PATH);

        let manager_public_key_or: Option<PublicKey> =
            match std::fs::read_to_string(&manager_public_key_path) {
                Ok(manager_public_key_str) => serde_json::from_str(&manager_public_key_str).ok(),
                Err(_) => None,
            };

        let (tx, rx) = mpsc::channel(1);
        let handler = ClaimHandler {
            docs: shared_protocol.docs.clone(),
            app_storage_path: shared_protocol.app_storage_path.clone(),
            claim_request_sender: tx,
            claimed_manager_pubkey: Arc::new(Mutex::new(manager_public_key_or)),
        };

        shared_protocol.router_builder = shared_protocol.router_builder.accept(CLAIM_ALPN, handler);

        Ok(Self {
            router: shared_protocol.router_builder.spawn().await?,
            blobs: shared_protocol.blobs,
            docs: shared_protocol.docs,
            app_storage_path: shared_protocol.app_storage_path,
            claim_request_receiver: rx,
        })
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.router.shutdown().await
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.router.is_shutdown()
    }

    pub async fn node_addr(&self) -> anyhow::Result<NodeAddr> {
        self.router.endpoint().node_addr().await
    }

    pub async fn await_next_incoming_claim_request(
        &mut self,
    ) -> anyhow::Result<(u32, oneshot::Sender<bool>)> {
        self.claim_request_receiver
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("claim channel closed"))
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
        federation_id: &FederationId,
        incoming_contract: &IncomingContract,
    ) -> anyhow::Result<()> {
        let doc = self.get_or_create_machine_doc().await?;

        let key = SharedProtocol::create_incoming_contract_machine_doc_key(
            federation_id,
            incoming_contract,
        );

        doc.set_bytes(
            self.docs.client().authors().default().await?,
            key,
            bincode::serde::encode_to_vec(incoming_contract, bincode::config::standard()).unwrap(),
        )
        .await
        .unwrap();
        Ok(())
    }

    pub async fn get_machine_config(&self) -> anyhow::Result<Option<MachineConfig>> {
        let Some(entry) = self
            .get_or_create_machine_doc()
            .await?
            .get_one(Query::key_exact(FEDERATION_INVITE_CODE_KEY))
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

    /// Creates an iroh doc (or returns the existing one) for use in storing/transferring data
    /// related to payments received to the current device. Should only be called on a machine.
    async fn get_or_create_machine_doc(&self) -> anyhow::Result<Doc<FlumeConnector<RpcService>>> {
        get_or_create_machine_doc(&self.app_storage_path, &self.docs).await
    }
}

// TODO: Perform this with a lock to prevent race conditions.
async fn get_or_create_machine_doc(
    app_storage_path: &Path,
    docs: &Docs<iroh_blobs::store::fs::Store>,
) -> anyhow::Result<Doc<FlumeConnector<RpcService>>> {
    let doc_ticket_path = app_storage_path.join(MACHINE_DOC_TICKET_PATH);

    let doc_ticket_or: Option<DocTicket> = match std::fs::read_to_string(&doc_ticket_path) {
        Ok(doc_ticket_str) => serde_json::from_str(&doc_ticket_str).ok(),
        Err(_) => None,
    };

    if let Some(doc_ticket) = doc_ticket_or {
        return docs
            .client()
            .open(doc_ticket.capability.id())
            .await
            .map(|doc_or| doc_or.unwrap());
    }

    let new_doc = docs.client().create().await?;

    // Save the doc ticket to a file for later use.
    let new_doc_ticket = new_doc.share(ShareMode::Write, AddrInfoOptions::Id).await?;
    let new_doc_ticket_str = serde_json::to_string(&new_doc_ticket).unwrap();
    std::fs::write(&doc_ticket_path, new_doc_ticket_str).unwrap();

    Ok(new_doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_machine_protocol() -> anyhow::Result<()> {
        let storage_path = tempfile::tempdir().unwrap();
        let machine_protocol = MachineProtocol::new(storage_path.path()).await?;

        let doc_ticket = machine_protocol
            .share_machine_doc(ShareMode::Write, AddrInfoOptions::Id)
            .await?;

        // Shutdown and restart to test basic persistence.
        machine_protocol.shutdown().await?;
        drop(machine_protocol);

        let machine_protocol = MachineProtocol::new(storage_path.path()).await?;

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
