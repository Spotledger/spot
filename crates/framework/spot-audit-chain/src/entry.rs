//! [`ChainEntry`]: a record linked into the chain.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::canonical::put_field;
use crate::record::AuditRecord;

/// A SHA-256 hash.
pub type Hash = [u8; 32];

/// The chain format version; part of the genesis hash.
pub const FORMAT_VERSION: u32 = 1;

const ENTRY_TAG: &[u8] = b"spot.audit.entry.v1\0";
const GENESIS_TAG: &[u8] = b"spot.audit.genesis.v1\0";

/// The `prev_hash` of the first entry of `site`'s chain.
#[must_use]
pub fn genesis_hash(site: &str) -> Hash {
    let mut bytes = GENESIS_TAG.to_vec();
    put_field(&mut bytes, site.as_bytes());
    bytes.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    Sha256::digest(&bytes).into()
}

/// One entry of the chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainEntry {
    /// Position in the chain; starts at 1 and has no gaps.
    pub seq: u64,
    /// The previous entry's hash; for entry 1, the site's genesis hash.
    pub prev_hash: Hash,
    /// What is audited.
    pub record: AuditRecord,
    /// `SHA-256(entry tag || seq || prev_hash || canonical(record))`.
    pub hash: Hash,
}

impl ChainEntry {
    /// Links `record` after `prev_hash` at `seq` and computes its hash.
    #[must_use]
    pub fn new(seq: u64, prev_hash: Hash, record: AuditRecord) -> Self {
        let hash = entry_hash(seq, &prev_hash, &record);
        Self {
            seq,
            prev_hash,
            record,
            hash,
        }
    }

    /// The hash the entry's current contents produce (compare with [`ChainEntry::hash`]).
    #[must_use]
    pub fn recompute_hash(&self) -> Hash {
        entry_hash(self.seq, &self.prev_hash, &self.record)
    }
}

fn entry_hash(seq: u64, prev_hash: &Hash, record: &AuditRecord) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(ENTRY_TAG);
    hasher.update(seq.to_be_bytes());
    hasher.update(prev_hash);
    hasher.update(record.canonical_bytes());
    hasher.finalize().into()
}
