use std::path::{Path, PathBuf};

use fedimint_core::{config::FederationId, db::DatabaseValue, invite_code::InviteCode};
use fedimint_lnv2_common::contracts::IncomingContract;
use iroh::{
    Endpoint, SecretKey,
    endpoint::Connection,
    protocol::{Router, RouterBuilder},
};
use iroh_blobs::{ALPN as BLOBS_ALPN, net_protocol::Blobs};
use iroh_docs::{ALPN as DOCS_ALPN, AuthorId, protocol::Docs};
use iroh_gossip::{ALPN as GOSSIP_ALPN, net::Gossip};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

const INCOMING_CONTRACT_PREFIX: [u8; 2] = [0x01, 0xFF];
pub const FEDERATION_INVITE_CODE_KEY: [u8; 2] = [0x02, 0xFF];
pub const CLAIM_ALPN: &[u8] = b"machine-claim/0";
const CLAIM_EXPORT_LABEL: &[u8] = b"machine-claim-pin";

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct MachineConfig {
    pub federation_invite_code: InviteCode,
}

const IROH_SUBDIR: &str = "iroh";
const APP_SUBDIR: &str = "app";
const SECRET_KEY_FILE: &str = "secret.key";

pub struct SharedProtocol {
    pub router_builder: RouterBuilder,
    pub blobs: Blobs<iroh_blobs::store::fs::Store>,
    pub docs: Docs<iroh_blobs::store::fs::Store>,
    pub app_storage_path: PathBuf,
}

impl SharedProtocol {
    pub async fn new(storage_path: &Path) -> anyhow::Result<Self> {
        let secret_key_path = storage_path.join(SECRET_KEY_FILE);
        let secret_key = if let Ok(key_str) = std::fs::read_to_string(&secret_key_path) {
            SecretKey::from_str(key_str.trim())?
        } else {
            let key = SecretKey::generate(OsRng);
            std::fs::write(&secret_key_path, key.to_string())?;
            key
        };

        let endpoint = Endpoint::builder()
            .secret_key(secret_key)
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

        let router_builder = builder
            .accept(BLOBS_ALPN, blobs.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(DOCS_ALPN, docs.clone());

        Ok(Self {
            router_builder,
            blobs,
            docs,
            app_storage_path,
        })
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
}

/// Derive a numeric claim PIN from exported keying material.
pub fn claim_pin_from_keying_material(connection: &Connection) -> u32 {
    let mut km = [0u8; 32];
    connection
        .export_keying_material(&mut km, CLAIM_EXPORT_LABEL, b"")
        .expect("Only fails if output length is too large, which it isn't");
    u32::from_be_bytes(km[..4].try_into().unwrap()) % 1_000_000
}
