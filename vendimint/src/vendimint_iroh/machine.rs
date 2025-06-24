use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use fedimint_core::config::FederationId;
use fedimint_lnv2_remote_client::ClaimableContract;
use futures_util::StreamExt;
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
    store::{QueryBuilder, SingleLatestPerKeyQuery},
    sync::Entry,
};
use n0_future::boxed::BoxFuture;
use quic_rpc::client::FlumeConnector;
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, mpsc, oneshot},
};

use crate::vendimint_iroh::shared::PING_MAGIC_BYTES;

use super::shared::{
    CLAIM_ALPN, KV_PREFIX, MACHINE_CONFIG_KEY, MachineConfig, SharedProtocol,
    claim_pin_from_keying_material,
};

const MACHINE_DOC_TICKET_PATH: &str = "machine_doc_ticket.json";
const MACHINE_MANAGER_PUBLIC_KEY_PATH: &str = "machine_manager_public_key.json";

pub struct MachineProtocol {
    router: Router,
    blobs: Blobs<iroh_blobs::store::fs::Store>,
    docs: Docs<iroh_blobs::store::fs::Store>,
    app_storage_path: PathBuf,
    claim_request_receiver: Mutex<mpsc::Receiver<(u32, oneshot::Sender<bool>)>>,
    #[cfg(test)]
    claimed_manager_pubkey: Arc<Mutex<Option<PublicKey>>>,
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
                let (mut send, mut recv) = connection.accept_bi().await?;

                // Manager sends some magic bytes to indicate
                // that it would like to claim this machine.
                // Read n + 1 bytes to ensure the magic byte
                // is the exact correct length, and no more.
                if recv.read_to_end(PING_MAGIC_BYTES.len() + 1).await? != PING_MAGIC_BYTES {
                    return Err(anyhow::anyhow!("Invalid manager stream open ping"));
                }

                let doc = get_or_create_machine_doc(&this.app_storage_path, &this.docs).await?;
                let ticket = doc
                    .share(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
                    .await?;

                let manager_public_key_path =
                    this.app_storage_path.join(MACHINE_MANAGER_PUBLIC_KEY_PATH);
                let claimer_pubkey_str = serde_json::to_string(&claimer_pubkey).unwrap();
                tokio::fs::write(&manager_public_key_path, claimer_pubkey_str)
                    .await
                    .unwrap();
                *claimed_manager_pubkey_lock = Some(claimer_pubkey);
                drop(claimed_manager_pubkey_lock);

                let ticket_bytes = serde_json::to_vec(&ticket)?;
                send.write_all(&ticket_bytes).await?;
                send.finish()?;
                send.stopped().await?;
                send.shutdown().await?;
                connection.close(0u32.into(), b"finished");
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
            match tokio::fs::read_to_string(&manager_public_key_path).await {
                Ok(manager_public_key_str) => serde_json::from_str(&manager_public_key_str).ok(),
                Err(_) => None,
            };

        let claimed_manager_pubkey = Arc::new(Mutex::new(manager_public_key_or));

        let (tx, rx) = mpsc::channel(1);

        let handler = ClaimHandler {
            docs: shared_protocol.docs.clone(),
            app_storage_path: shared_protocol.app_storage_path.clone(),
            claim_request_sender: tx,
            claimed_manager_pubkey: claimed_manager_pubkey.clone(),
        };

        shared_protocol.router_builder = shared_protocol.router_builder.accept(CLAIM_ALPN, handler);

        Ok(Self {
            router: shared_protocol.router_builder.spawn().await?,
            blobs: shared_protocol.blobs,
            docs: shared_protocol.docs,
            app_storage_path: shared_protocol.app_storage_path,
            claim_request_receiver: Mutex::new(rx),
            #[cfg(test)]
            claimed_manager_pubkey,
        })
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.router.shutdown().await?;
        self.blobs.shutdown().await;
        self.docs.shutdown().await;
        Ok(())
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.router.is_shutdown()
    }

    pub async fn node_addr(&self) -> anyhow::Result<NodeAddr> {
        self.router.endpoint().node_addr().await
    }
    pub async fn await_next_incoming_claim_request(&self) -> Option<(u32, oneshot::Sender<bool>)> {
        self.claim_request_receiver.lock().await.recv().await
    }

    pub async fn write_payment_to_machine_doc(
        &self,
        federation_id: &FederationId,
        claimable_contract: &ClaimableContract,
    ) -> anyhow::Result<()> {
        let doc = self.get_or_create_machine_doc().await?;

        let key = SharedProtocol::create_claimable_contract_machine_doc_key(
            federation_id,
            claimable_contract,
        );

        doc.set_bytes(
            self.docs.client().authors().default().await?,
            key,
            serde_json::to_vec(claimable_contract)?,
        )
        .await?;

        Ok(())
    }

    pub async fn get_machine_config(&self) -> anyhow::Result<Option<MachineConfig>> {
        let Some(entry) = self
            .get_or_create_machine_doc()
            .await?
            .get_one(
                QueryBuilder::<SingleLatestPerKeyQuery>::default().key_exact(MACHINE_CONFIG_KEY),
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

    /// Creates an iroh doc (or returns the existing one) for use in storing/transferring data
    /// related to payments received to the current device. Should only be called on a machine.
    async fn get_or_create_machine_doc(&self) -> anyhow::Result<Doc<FlumeConnector<RpcService>>> {
        get_or_create_machine_doc(&self.app_storage_path, &self.docs).await
    }

    pub async fn get_kv_value(&self, key: impl AsRef<[u8]>) -> anyhow::Result<Option<Entry>> {
        let doc = self.get_or_create_machine_doc().await?;

        let mut full_key = KV_PREFIX.to_vec();
        full_key.extend_from_slice(key.as_ref());

        doc.get_one(QueryBuilder::<SingleLatestPerKeyQuery>::default().key_exact(full_key))
            .await
    }

    pub async fn set_kv_value(
        &self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
    ) -> anyhow::Result<()> {
        let doc = self.get_or_create_machine_doc().await?;

        let mut full_key = KV_PREFIX.to_vec();
        full_key.extend_from_slice(key.as_ref());

        doc.set_bytes(
            self.docs.client().authors().default().await?,
            full_key,
            value.as_ref().to_vec(),
        )
        .await?;

        Ok(())
    }

    pub async fn get_kv_entries(&self) -> anyhow::Result<Vec<Entry>> {
        let doc = self.get_or_create_machine_doc().await?;

        let mut entries = Vec::new();
        let mut entry_stream = doc
            .get_many(
                QueryBuilder::<SingleLatestPerKeyQuery>::default()
                    .key_prefix(KV_PREFIX)
                    .build(),
            )
            .await?;

        while let Some(entry_result) = entry_stream.next().await {
            entries.push(entry_result?);
        }

        Ok(entries)
    }

    #[cfg(test)]
    pub async fn get_manager_pubkey(&self) -> Option<PublicKey> {
        *self.claimed_manager_pubkey.lock().await
    }
}

// TODO: Perform this with a lock to prevent race conditions.
async fn get_or_create_machine_doc(
    app_storage_path: &Path,
    docs: &Docs<iroh_blobs::store::fs::Store>,
) -> anyhow::Result<Doc<FlumeConnector<RpcService>>> {
    tokio::fs::create_dir_all(app_storage_path).await.unwrap();

    let doc_ticket_path = app_storage_path.join(MACHINE_DOC_TICKET_PATH);

    let doc_ticket_or: Option<DocTicket> = match tokio::fs::read_to_string(&doc_ticket_path).await {
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
    tokio::fs::write(&doc_ticket_path, new_doc_ticket_str)
        .await
        .unwrap();

    Ok(new_doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_machine_protocol() -> anyhow::Result<()> {
        let storage_path = tempfile::tempdir().unwrap();
        let machine_protocol = MachineProtocol::new(storage_path.path()).await?;

        let node_addr = machine_protocol.node_addr().await?;

        // Shutdown and restart to test basic persistence.
        machine_protocol.shutdown().await?;
        drop(machine_protocol);

        let machine_protocol = MachineProtocol::new(storage_path.path()).await?;

        assert_eq!(
            machine_protocol.node_addr().await?.node_id,
            node_addr.node_id
        );

        Ok(())
    }
}
