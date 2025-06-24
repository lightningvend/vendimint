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
pub const MACHINE_CONFIG_KEY: [u8; 2] = [0x02, 0xFF];
/// Prefix for generic key/value storage. Used to store arbitrary data
/// that has meaning to the API caller, but no meaning within vendimint.
pub const KV_PREFIX: [u8; 2] = [0x03, 0xFF];
pub const CLAIM_ALPN: &[u8] = b"machine-claim/0";
const CLAIM_EXPORT_LABEL: &[u8] = b"machine-claim-pin";

/// Magic bytes sent from manager to machine to alert
/// the machine that the manager would like to claim it.
pub const PING_MAGIC_BYTES: [u8; 4] = [0x01, 0x02, 0x03, 0x04];

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
pub struct MachineConfig {
    pub federation_invite_code: InviteCode,
    pub claimer_pk: PublicKey,
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
        let secret_key = if let Ok(key_str) = tokio::fs::read_to_string(&secret_key_path).await {
            SecretKey::from_str(key_str.trim())?
        } else {
            let key = SecretKey::generate(OsRng);
            tokio::fs::create_dir_all(storage_path).await.unwrap();
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

        tokio::fs::create_dir_all(&iroh_storage_path).await.unwrap();
        tokio::fs::create_dir_all(&app_storage_path).await.unwrap();

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

        let federation_id_bytes: [u8; 32] = key[2..34].try_into().unwrap();
        let contract_id_hash_bytes: [u8; 32] = key[34..66].try_into().unwrap();

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
