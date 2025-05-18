use std::{
    path::PathBuf,
    sync::{atomic::{AtomicBool, Ordering}, Arc},
};

use fedimint_core::config::FederationId;
use fedimint_lnv2_common::contracts::IncomingContract;
use iroh::{
    endpoint::{Connection},
    protocol::{ProtocolHandler, Router},
    NodeAddr,
};
use iroh_blobs::ALPN as BLOBS_ALPN;
use iroh_docs::ALPN as DOCS_ALPN;
use iroh_gossip::ALPN as GOSSIP_ALPN;
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
use tokio::sync::{mpsc, oneshot};

use shared::{
    CLAIM_ALPN, CLAIM_EXPORT_LABEL, FEDERATION_INVITE_CODE_KEY, MachineConfig,
    SharedProtocol,
};

const MACHINE_DOC_TICKET_PATH: &str = "machine_doc_ticket.json";

pub struct MachineProtocol {
    shared_protocol: SharedProtocol,
    claim_router: Router,
    claim_requests: mpsc::Receiver<(u32, oneshot::Sender<bool>)>,
    claimed: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
struct ClaimHandler {
    shared_protocol: SharedProtocol,
    requests: mpsc::Sender<(u32, oneshot::Sender<bool>)>,
    claimed: Arc<AtomicBool>,
}

impl ProtocolHandler for ClaimHandler {
    fn accept(&self, conn: Connection) -> BoxFuture<anyhow::Result<()>> {
        let this = self.clone();
        Box::pin(async move {
            if this.claimed.load(Ordering::SeqCst) {
                conn.close(0u32.into(), b"claimed");
                return Ok(());
            }
            let mut km = [0u8; 32];
            conn
                .export_keying_material(&mut km, CLAIM_EXPORT_LABEL, b"")
                .map_err(|e| anyhow::anyhow!(format!("{:?}", e)))?;
            let pin = SharedProtocol::claim_pin_from_keying_material(&km);
            let (tx, rx) = oneshot::channel();
            this.requests
                .send((pin, tx))
                .await
                .map_err(|_| anyhow::anyhow!("receiver dropped"))?;
            if rx.await.unwrap_or(false) {
                let doc = get_or_create_machine_doc(&this.shared_protocol).await?;
                let ticket = doc
                    .share(ShareMode::Write, AddrInfoOptions::Id)
                    .await?;
                let bytes = serde_json::to_vec(&ticket)?;
                let mut send = conn.open_uni().await?;
                send.write_all(&bytes).await?;
                send.finish()?;
                this.claimed.store(true, Ordering::SeqCst);
            }
            conn.close(0u32.into(), b"done");
            Ok(())
        })
    }

    fn shutdown(&self) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

impl MachineProtocol {
    pub async fn new(storage_path: PathBuf) -> anyhow::Result<Self> {
        let shared_protocol = SharedProtocol::new(storage_path).await?;

        let endpoint = shared_protocol.endpoint().clone();
        let (tx, rx) = mpsc::channel(1);
        let claimed = Arc::new(AtomicBool::new(false));
        let handler = ClaimHandler {
            shared_protocol: shared_protocol.clone(),
            requests: tx.clone(),
            claimed: claimed.clone(),
        };
        let claim_router = Router::builder(endpoint.clone())
            .accept(CLAIM_ALPN, handler)
            .spawn()
            .await?;
        endpoint
            .set_alpns(vec![
                BLOBS_ALPN.to_vec(),
                GOSSIP_ALPN.to_vec(),
                DOCS_ALPN.to_vec(),
                CLAIM_ALPN.to_vec(),
            ])
            .unwrap();

        Ok(Self {
            shared_protocol,
            claim_router,
            claim_requests: rx,
            claimed,
        })
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.claim_router.shutdown().await?;
        self.shared_protocol.shutdown().await
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.shared_protocol.is_shutdown() && self.claim_router.is_shutdown()
    }

    pub async fn node_addr(&self) -> anyhow::Result<NodeAddr> {
        Ok(self.shared_protocol.endpoint().node_addr().await?)
    }

    pub async fn await_next_incoming_claim_request(
        &mut self,
    ) -> anyhow::Result<(u32, oneshot::Sender<bool>)> {
        let req = self
            .claim_requests
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("claim channel closed"))?;
        Ok(req)
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
            self.shared_protocol.get_author_id().await?,
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
            .shared_protocol
            .get_blobs()
            .client()
            .read_to_bytes(entry.content_hash())
            .await?;

        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Creates an iroh doc (or returns the existing one) for use in storing/transferring data
    /// related to payments received to the current device. Should only be called on a machine.
    async fn get_or_create_machine_doc(&self) -> anyhow::Result<Doc<FlumeConnector<RpcService>>> {
        get_or_create_machine_doc(&self.shared_protocol).await
    }
}

async fn get_or_create_machine_doc(
    shared_protocol: &SharedProtocol,
) -> anyhow::Result<Doc<FlumeConnector<RpcService>>> {
    let doc_ticket_path =
        shared_protocol.get_app_storage_path().join(MACHINE_DOC_TICKET_PATH);

    let doc_ticket_or: Option<DocTicket> =
        match std::fs::read_to_string(&doc_ticket_path) {
            Ok(doc_ticket_str) => serde_json::from_str(&doc_ticket_str).ok(),
            Err(_) => None,
        };

    if let Some(doc_ticket) = doc_ticket_or {
        return shared_protocol
            .get_docs()
            .client()
            .open(doc_ticket.capability.id())
            .await
            .map(|doc_or| doc_or.unwrap());
    }

    let new_doc = shared_protocol.get_docs().client().create().await?;

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
