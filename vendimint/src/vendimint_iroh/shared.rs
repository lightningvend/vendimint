use std::path::{Path, PathBuf};

use fedimint_core::{
    config::FederationId, db::DatabaseValue, invite_code::InviteCode, secp256k1::PublicKey,
};
use fedimint_lnv2_common::ContractId;
use fedimint_lnv2_remote_client::ClaimableContract;
use iroh::{
    Endpoint, SecretKey,
    endpoint::Connection,
    protocol::{Router, RouterBuilder},
};
use iroh_blobs::{ALPN as BLOBS_ALPN, net_protocol::Blobs};
use iroh_docs::{ALPN as DOCS_ALPN, protocol::Docs};
use iroh_gossip::{ALPN as GOSSIP_ALPN, net::Gossip};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub const CLAIMABLE_CONTRACT_PREFIX: [u8; 2] = [0x01, 0xFF];
/// Key for storing the machine's configuration.
/// A value for this key is set by the machine when it
/// is claimed, after which a value for this key must
/// _always_ be present.
pub const MACHINE_CONFIG_KEY: [u8; 2] = [0x02, 0xFF];
/// Prefix for generic key/value storage. Used to store arbitrary data
/// that has meaning to the API caller, but no meaning within vendimint.
pub const KV_PREFIX: [u8; 2] = [0x03, 0xFF];
pub const CLAIM_ALPN: &[u8] = b"machine-claim/0";
const CLAIM_EXPORT_LABEL: &[u8] = b"machine-claim-pin";

/// Max size of the machine config that the manager can
/// send to the machine at claim time. Limits worst-case
/// memory usage by iroh. Generously set to 1MB, which
/// should be much more than enough for any reasonable
/// configuration.
pub const CLAIM_MAX_MACHINE_CONFIG_SIZE_BYTES: usize = 1024 * 1024;

const IROH_SUBDIR: &str = "iroh";
const APP_SUBDIR: &str = "app";
const SECRET_KEY_FILE: &str = "secret.key";

/// A machine's configuration, which determines how funds are received.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct MachineConfig {
    pub federation_invite_code: InviteCode,
    pub claimer_pk: PublicKey,
}

/// A key/value pair in the per-machine shared KV store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvEntry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub author: KvEntryAuthor,
    pub timestamp: u64,
}

/// Indicates whether a [`KvEntry`] was written by the machine or its manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvEntryAuthor {
    Manager,
    Machine,
}

impl KvEntry {
    pub(crate) async fn from_iroh_entry(
        entry: iroh_docs::sync::Entry,
        reader_device: KvEntryAuthor,
        blobs: &iroh_blobs::net_protocol::Blobs<iroh_blobs::store::fs::Store>,
        docs: &iroh_docs::protocol::Docs<iroh_blobs::store::fs::Store>,
    ) -> anyhow::Result<Self> {
        let value = blobs.client().read_to_bytes(entry.content_hash()).await?;

        let full_key = entry.key();
        let user_key = if full_key.starts_with(&KV_PREFIX) {
            full_key[KV_PREFIX.len()..].to_vec()
        } else {
            full_key.to_vec()
        };

        // Determine the original author of the entry.
        // Only two devices can write to a given KV store: a machine
        // and its manager. If the current device didn't originally
        // create the entry, then the other device must have.
        let author = if entry.author() == docs.client().authors().default().await? {
            reader_device
        } else {
            // The author must be the opposite of `reader_device`.
            match reader_device {
                KvEntryAuthor::Machine => KvEntryAuthor::Manager,
                KvEntryAuthor::Manager => KvEntryAuthor::Machine,
            }
        };

        Ok(Self {
            key: user_key,
            value: value.to_vec(),
            author,
            timestamp: entry.timestamp(),
        })
    }
}

pub struct SharedProtocol {
    pub router_builder: RouterBuilder,
    pub blobs: Blobs<iroh_blobs::store::fs::Store>,
    pub docs: Docs<iroh_blobs::store::fs::Store>,
    pub app_storage_path: PathBuf,
}

impl SharedProtocol {
    pub async fn new(storage_path: &Path) -> anyhow::Result<Self> {
        let secret_key_path = storage_path.join(SECRET_KEY_FILE);
        let secret_key = if let Ok(key_str) = tokio::fs::read_to_string(&secret_key_path).await {
            SecretKey::from_str(key_str.trim())?
        } else {
            let key = SecretKey::generate(OsRng);
            tokio::fs::create_dir_all(storage_path).await?;
            tokio::fs::write(&secret_key_path, key.to_string()).await?;
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

        tokio::fs::create_dir_all(&iroh_storage_path).await?;
        tokio::fs::create_dir_all(&app_storage_path).await?;

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

    pub fn create_claimable_contract_machine_doc_key(
        federation_id: &FederationId,
        claimable_contract: &ClaimableContract,
    ) -> Vec<u8> {
        let mut key = CLAIMABLE_CONTRACT_PREFIX.to_vec();
        key.append(&mut federation_id.0.to_bytes());
        let contract_id_hash_bytes: [u8; 32] =
            *claimable_contract.contract.contract_id().0.as_ref();
        key.append(&mut contract_id_hash_bytes.to_vec());
        key
    }

    pub fn parse_incoming_contract_machine_doc_key(
        key: &[u8],
    ) -> anyhow::Result<(FederationId, ContractId)> {
        if key.len() != 66 {
            return Err(anyhow::anyhow!("Invalid key length"));
        }

        if !key.starts_with(&CLAIMABLE_CONTRACT_PREFIX) {
            return Err(anyhow::anyhow!("Invalid key prefix"));
        }

        let federation_id_bytes: [u8; 32] = key[2..34].try_into()?;
        let contract_id_hash_bytes: [u8; 32] = key[34..66].try_into()?;

        let federation_id = FederationId(*bitcoin::hashes::sha256::Hash::from_bytes_ref(
            &federation_id_bytes,
        ));
        let contract_id = ContractId(*bitcoin::hashes::sha256::Hash::from_bytes_ref(
            &contract_id_hash_bytes,
        ));
        Ok((federation_id, contract_id))
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
