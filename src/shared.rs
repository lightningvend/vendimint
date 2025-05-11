use std::path::{Path, PathBuf};

use fedimint_core::{config::FederationId, db::DatabaseValue};
use fedimint_lnv2_common::contracts::IncomingContract;
use iroh::{Endpoint, protocol::Router};
use iroh_blobs::{ALPN as BLOBS_ALPN, net_protocol::Blobs};
use iroh_docs::{ALPN as DOCS_ALPN, AuthorId, protocol::Docs};
use iroh_gossip::{ALPN as GOSSIP_ALPN, net::Gossip};

const INCOMING_CONTRACT_PREFIX: [u8; 2] = [0x01, 0xFF];

const IROH_SUBDIR: &str = "iroh";
const APP_SUBDIR: &str = "app";

pub struct SharedProtocol {
    router: Router,
    docs: Docs<iroh_blobs::store::fs::Store>,
    app_storage_path: PathBuf,
}

impl SharedProtocol {
    pub async fn new(storage_path: PathBuf) -> anyhow::Result<Self> {
        let endpoint = Endpoint::builder()
            .discovery_n0()
            .discovery_local_network()
            .bind()
            .await?;

        let builder = Router::builder(endpoint);

        let iroh_storage_path = storage_path.join(IROH_SUBDIR);
        let app_storage_path = storage_path.join(APP_SUBDIR);

        std::fs::create_dir_all(&iroh_storage_path).unwrap();
        std::fs::create_dir_all(&app_storage_path).unwrap();

        let blobs = Blobs::persistent(&iroh_storage_path)
            .await?
            .build(builder.endpoint());

        let gossip = Gossip::builder().spawn(builder.endpoint().clone()).await?;

        let docs = Docs::persistent(iroh_storage_path)
            .spawn(&blobs, &gossip)
            .await?;

        let router = builder
            .accept(BLOBS_ALPN, blobs.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(DOCS_ALPN, docs.clone())
            .spawn()
            .await?;

        Ok(Self {
            router,
            docs,
            app_storage_path,
        })
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.router.shutdown().await
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.router.is_shutdown()
    }

    pub async fn get_author_id(&self) -> anyhow::Result<AuthorId> {
        self.docs.client().authors().default().await
    }

    pub fn create_incoming_contract_machine_doc_key(
        federation_id: &FederationId,
        incoming_contract: &IncomingContract,
    ) -> Vec<u8> {
        let mut key = INCOMING_CONTRACT_PREFIX.to_vec();
        key.append(&mut federation_id.0.to_bytes());
        let contract_id_hash_bytes: [u8; 32] = *incoming_contract.contract_id().0.as_ref();
        key.append(&mut contract_id_hash_bytes.to_vec());
        key
    }

    pub fn get_docs(&self) -> &Docs<iroh_blobs::store::fs::Store> {
        &self.docs
    }

    pub fn get_app_storage_path(&self) -> &Path {
        self.app_storage_path.as_path()
    }
}
