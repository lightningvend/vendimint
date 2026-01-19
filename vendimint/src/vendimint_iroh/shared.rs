use std::path::{Path, PathBuf};

use fedimint_core::{
    config::FederationId, db::DatabaseValue, invite_code::InviteCode, secp256k1::PublicKey,
};
use fedimint_lnv2_common::ContractId;
use fedimint_lnv2_remote_client::ClaimableContract;
use iroh::{
    EndpointAddr, Endpoint, SecretKey,
    endpoint::Connection,
    protocol::{Router, RouterBuilder},
};
use iroh_blobs::{ALPN as BLOBS_ALPN, BlobsProtocol, store::fs::FsStore};
use iroh_docs::{ALPN as DOCS_ALPN, protocol::Docs};
use iroh_gossip::{ALPN as GOSSIP_ALPN, net::Gossip};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};

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
        blobs: &BlobsProtocol,
        docs: &Docs,
    ) -> anyhow::Result<Self> {
        let value = blobs.get_bytes(entry.content_hash()).await?;

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
        let author = if entry.author() == docs.author_default().await? {
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
    pub blobs: BlobsProtocol,
    pub docs: Docs,
    pub app_storage_path: PathBuf,
}

impl SharedProtocol {
    pub async fn new(storage_path: &Path) -> anyhow::Result<Self> {
        let secret_key_path = storage_path.join(SECRET_KEY_FILE);
        let secret_key = if let Ok(key_bytes) = tokio::fs::read(&secret_key_path).await {
            // TODO: Handle `.unwrap()`.
            SecretKey::from_bytes(&key_bytes.try_into().unwrap())
        } else {
            let mut rng = OsRng;
            let mut key_bytes = [0u8; 32];
            rng.fill_bytes(&mut key_bytes);
            tokio::fs::create_dir_all(storage_path).await?;
            tokio::fs::write(&secret_key_path, &key_bytes).await?;
            SecretKey::from_bytes(&key_bytes)
        };

        let endpoint = Endpoint::builder().secret_key(secret_key).bind().await?;

        let builder = Router::builder(endpoint);

        let iroh_storage_path = storage_path.join(IROH_SUBDIR);
        let app_storage_path = storage_path.join(APP_SUBDIR);

        tokio::fs::create_dir_all(&iroh_storage_path).await?;
        tokio::fs::create_dir_all(&app_storage_path).await?;

        let blobs_store = FsStore::load(&iroh_storage_path).await?;
        let blobs = BlobsProtocol::new(&blobs_store, None);

        let gossip = Gossip::builder().spawn(builder.endpoint().clone());

        let docs = Docs::persistent(iroh_storage_path.clone())
            .spawn(
                builder.endpoint().clone(),
                blobs.store().clone(),
                gossip.clone(),
            )
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

/// A claim PIN used to verify machine claiming operations.
/// The PIN is always a 6-digit number (0-999999).
/// The `Display` implementation zero-pads the PIN to 6 digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClaimPin(u32);

impl ClaimPin {
    /// Get the raw u32 value of the PIN.
    #[must_use]
    pub const fn as_u32(&self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for ClaimPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Zero-pad to 6 digits.
        write!(f, "{:06}", self.0)
    }
}

/// Derive a numeric claim PIN from exported keying material.
pub fn claim_pin_from_keying_material(connection: &Connection) -> ClaimPin {
    let mut km = [0u8; 32];
    connection
        .export_keying_material(&mut km, CLAIM_EXPORT_LABEL, b"")
        .expect("Only fails if output length is too large, which it isn't");

    // Note: It's important that we modulo the PIN to ensure it's within the valid range.
    ClaimPin(u32::from_be_bytes(km[..4].try_into().unwrap()) % 1_000_000)
}

/// A newtype wrapper around [`EndpointAddr`] for machine claiming.
///
/// This type provides a user-friendly string representation of machine addresses
/// using JSON encoding, making it easy to copy/paste claim keys between systems.
///
/// # Encoding
///
/// The `Display` implementation serializes the inner `EndpointAddr` to JSON.
/// The `FromStr` implementation deserializes from JSON. This encoding was chosen
/// because:
/// - It's stable and human-readable
/// - It's compatible with the existing serde ecosystem
/// - `EndpointAddr` already implements `Serialize`/`Deserialize`
/// - No additional dependencies are required
///
/// # Examples
///
/// ```
/// # use vendimint::vendimint_iroh::shared::ClaimKey;
/// # use std::str::FromStr;
/// // Convert a ClaimKey to a string for sharing
/// let claim_key = /* ... */;
/// let claim_key_str = claim_key.to_string();
///
/// // Parse a ClaimKey from a string
/// let parsed_key = ClaimKey::from_str(&claim_key_str).unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimKey(EndpointAddr);

impl ClaimKey {
    /// Create a new `ClaimKey` from an `EndpointAddr`.
    #[must_use]
    pub const fn new(addr: EndpointAddr) -> Self {
        Self(addr)
    }

    /// Get the inner `EndpointAddr`.
    #[must_use]
    pub fn into_inner(self) -> EndpointAddr {
        self.0
    }

    /// Get a reference to the inner `EndpointAddr`.
    #[must_use]
    pub const fn as_inner(&self) -> &EndpointAddr {
        &self.0
    }
}

impl From<EndpointAddr> for ClaimKey {
    fn from(addr: EndpointAddr) -> Self {
        Self(addr)
    }
}

impl From<ClaimKey> for EndpointAddr {
    fn from(key: ClaimKey) -> Self {
        key.0
    }
}

impl std::fmt::Display for ClaimKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Serialize to JSON for a stable, human-readable representation.
        let json = serde_json::to_string(&self.0)
            .map_err(|_| std::fmt::Error)?;
        write!(f, "{}", json)
    }
}

impl std::str::FromStr for ClaimKey {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let addr = serde_json::from_str(s)?;
        Ok(Self(addr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claim_pin_display_zero_pads_to_six_digits() {
        let pin_zero = ClaimPin(0);
        let pin_small = ClaimPin(7);
        let pin_exact = ClaimPin(999_999);

        assert_eq!(pin_zero.to_string(), "000000");
        assert_eq!(pin_small.to_string(), "000007");
        assert_eq!(pin_exact.to_string(), "999999");
    }

    #[test]
    fn test_claim_pin_equality() {
        assert_eq!(ClaimPin(123_456), ClaimPin(123_456));
        assert_ne!(ClaimPin(123_456), ClaimPin(654_321));
    }

    #[test]
    fn test_claim_key_display_and_from_str_roundtrip() {
        use std::str::FromStr;
        use iroh::EndpointId;

        // Create a test EndpointAddr
        let endpoint_id = EndpointId::from_bytes(&[0u8; 32]);
        let addr = EndpointAddr::new(endpoint_id);
        let claim_key = ClaimKey::new(addr.clone());

        // Convert to string and back
        let claim_key_str = claim_key.to_string();
        let parsed_key = ClaimKey::from_str(&claim_key_str).unwrap();

        // Should be equal
        assert_eq!(claim_key, parsed_key);
        assert_eq!(claim_key.as_inner(), parsed_key.as_inner());
    }

    #[test]
    fn test_claim_key_from_endpoint_addr() {
        use iroh::EndpointId;

        let endpoint_id = EndpointId::from_bytes(&[1u8; 32]);
        let addr = EndpointAddr::new(endpoint_id);
        
        let claim_key: ClaimKey = addr.clone().into();
        assert_eq!(claim_key.as_inner(), &addr);
    }

    #[test]
    fn test_claim_key_into_endpoint_addr() {
        use iroh::EndpointId;

        let endpoint_id = EndpointId::from_bytes(&[2u8; 32]);
        let addr = EndpointAddr::new(endpoint_id);
        let claim_key = ClaimKey::new(addr.clone());
        
        let addr_back: EndpointAddr = claim_key.into();
        assert_eq!(addr, addr_back);
    }
}
