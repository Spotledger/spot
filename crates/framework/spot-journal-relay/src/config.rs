//! What the relay is configured with, and what a call takes and returns.

use spot_events_contract::envelope::Position;
use spot_journal_segment::SealLimits;
use spot_journal_store::TenantId;

use crate::error::RelayError;

/// How a commit becomes durable (ADR-061 s4). Mirrors the durability policies without depending
/// on the crate that decides acknowledgements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durability {
    /// The local database commit is the durability; the relay only ships. Entries wait in memory
    /// for the open segment and no tail chunk is written.
    Local,
    /// A synchronous standby is the durability; same relay behaviour as [`Durability::Local`].
    ReplicaSync,
    /// A tail chunk in the object store is the durability: [`crate::Relay::append_batch`] writes
    /// the chunk before it returns.
    RemoteSync,
}

/// The configuration of one relay: one tenant, one source, one durability policy.
#[derive(Clone, Debug)]
pub struct RelayConfig {
    /// The tenant id; fixed for the life of the relay and used for every key.
    pub tenant: String,
    /// The journal source (a tenant site, or `<site>#<partition>`).
    pub source: String,
    /// How a batch becomes durable.
    pub durability: Durability,
    /// When the open segment is sealed.
    pub limits: SealLimits,
}

impl RelayConfig {
    /// A configuration with the default sealing limits.
    #[must_use]
    pub fn new(tenant: &str, source: &str, durability: Durability) -> Self {
        Self {
            tenant: tenant.to_owned(),
            source: source.to_owned(),
            durability,
            limits: SealLimits::default(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), RelayError> {
        TenantId::new(&self.tenant).map_err(|_| {
            RelayError::ConfigInvalid("the tenant id is not one safe path component")
        })?;
        if self.source.trim().is_empty() {
            return Err(RelayError::ConfigInvalid("the source id is empty"));
        }
        Ok(())
    }
}

/// One batch of entries for the relay: strictly consecutive positions of the relay's generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    /// The batch id; strictly increasing across the batches one relay accepts. It names the tail
    /// chunk under remote-sync.
    pub batch_id: u64,
    /// The entries: a position and the envelope as one line of UTF-8 (opaque to the relay).
    pub entries: Vec<(Position, Vec<u8>)>,
}

/// What the relay promises once a batch is accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appended {
    /// A tail chunk holding the whole batch is durable in the store. This is the value the
    /// remote-sync acknowledgement gate consumes: every commit up to `up_to_seq` may be
    /// acknowledged.
    ChunkDurable {
        /// The batch that is durable.
        batch_id: u64,
        /// The last sequence number the batch holds.
        up_to_seq: u64,
    },
    /// The batch is held for the open segment and nothing was written (`local` and
    /// `replica-sync`: the durability is elsewhere). Not an acknowledgement.
    Buffered {
        /// The batch that is held.
        batch_id: u64,
        /// The last sequence number the batch holds.
        up_to_seq: u64,
    },
}
