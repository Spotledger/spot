//! The chunk as it crosses the wire: an index, a body and the hash of the body. The node verifies
//! the hash before it applies anything; a body that does not match is refused whole.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spot_sync_bootstrap::Chunk;
use spot_sync_catchup::{BundleSpec, CatchUpPlan, Event};

use crate::serve::ServeError;

/// What one catch-up chunk holds. The mandatory lane goes one event at a time and in order, then
/// the bulk bundles, then the audit events addressed to the node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatchUpChunk {
    /// One mandatory event.
    Mandatory(Event),
    /// One coalesced, dependency-ordered bundle.
    Bulk(BundleSpec),
    /// The audit events addressed to the node.
    Audit(Vec<Event>),
}

/// The body of a chunk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ChunkBody {
    /// Snapshot bootstrap: whole dependency groups of record images.
    Bootstrap(Chunk),
    /// Coalesced catch-up.
    CatchUp(CatchUpChunk),
}

/// One chunk on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireChunk {
    /// The chunk's index from 0.
    pub index: u64,
    /// Whether this is the last chunk of the plan.
    pub last: bool,
    /// For a bootstrap's last chunk: the position the node continues from on the live stream.
    pub resume_from: Option<u64>,
    /// Lower-case hex SHA-256 of `body`.
    pub hash: String,
    /// The JSON of a [`ChunkBody`].
    pub body: String,
}

fn hex_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl WireChunk {
    /// Seals `body` as chunk `index`.
    ///
    /// # Errors
    /// [`ServeError::Encode`] when the body cannot be written as JSON.
    pub fn seal(
        index: u64,
        last: bool,
        resume_from: Option<u64>,
        body: &ChunkBody,
    ) -> Result<Self, ServeError> {
        let body = serde_json::to_string(body).map_err(|_| ServeError::Encode)?;
        Ok(Self {
            index,
            last,
            resume_from,
            hash: hex_hash(&body),
            body,
        })
    }

    /// Checks the hash and decodes the body.
    ///
    /// # Errors
    /// [`ServeError::Tampered`] when the hash does not match or the body does not decode.
    pub fn open(&self) -> Result<ChunkBody, ServeError> {
        if hex_hash(&self.body) != self.hash {
            return Err(ServeError::Tampered);
        }
        serde_json::from_str(&self.body).map_err(|_| ServeError::Tampered)
    }
}

/// The chunks of a catch-up plan, in the order they are served: the mandatory lane event by
/// event, the bulk bundles, then the audit events (when there are any).
#[must_use]
pub fn catch_up_chunks(plan: &CatchUpPlan) -> Vec<CatchUpChunk> {
    let mut chunks: Vec<CatchUpChunk> = plan
        .mandatory_lane
        .iter()
        .cloned()
        .map(CatchUpChunk::Mandatory)
        .collect();
    chunks.extend(plan.bulk.iter().cloned().map(CatchUpChunk::Bulk));
    if !plan.audit.is_empty() {
        chunks.push(CatchUpChunk::Audit(plan.audit.clone()));
    }
    chunks
}
