//! The values the [`crate::Coordinator`] trades in: leases, fencing tokens, rate limits.

use std::time::Duration;

use time::OffsetDateTime;

use crate::error::{Error, INVALID_NAME, Result};

/// The longest name, in bytes.
pub const MAX_NAME_BYTES: usize = 200;

/// The longest ttl a lease may ask for. A ttl is the bound on how long a crashed holder blocks
/// everyone else, so an absurd one is a bug, not a request.
pub const MAX_TTL: Duration = Duration::from_secs(366 * 24 * 60 * 60);

/// Checks a lease, lock, counter or rate-limit name: non-empty, at most [`MAX_NAME_BYTES`]
/// bytes, and printable (no control characters; any other Unicode is fine).
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::new(INVALID_NAME, "a name must not be empty"));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(Error::new(
            INVALID_NAME,
            format!(
                "a name is at most {MAX_NAME_BYTES} bytes, this one has {}",
                name.len()
            ),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::new(
            INVALID_NAME,
            "a name must be printable: it has a control character",
        ));
    }
    Ok(())
}

/// A fencing token: one more than every token ever issued for the lease's name.
///
/// Same semantics as `spot_scheduler::FencingToken`: the first acquisition of a name gets 1 and
/// every later acquisition (after release or expiry) gets a strictly larger one, for the life of
/// the store, never reused. A store that wants to refuse a stale writer compares the token the
/// writer holds against the live one ([`crate::Coordinator::check_fence`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FencingToken(pub u64);

/// Which lease record a handle names: the record's issue counter, so two acquisitions by the
/// same holder (one from before a crash) renew or release their own record, never each other's.
pub type LeaseId = u64;

/// One lease: who holds it, until when, and the fencing token it writes under.
///
/// A `Lease` is a *handle*. Holding one proves nothing: the coordinator decides whether it is
/// still the current record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    /// The lease's name.
    pub name: String,
    /// Who holds it (a node id, a hostname or instance).
    pub holder: String,
    /// The fencing token this acquisition got.
    pub token: FencingToken,
    /// When the lease stops covering its holder. Half-open: at this instant it is over.
    pub expires_at: OffsetDateTime,
    /// The record this handle points at.
    pub id: LeaseId,
}

/// A rate limit: at most `capacity` units per `per`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RateLimit {
    /// How many units a full bucket holds, and how many may be taken in one `per`.
    pub capacity: u64,
    /// The window over which `capacity` units refill.
    pub per: Duration,
}

impl RateLimit {
    /// Checks that the limit can admit anything at all.
    pub fn validate(&self) -> Result<()> {
        if self.capacity == 0 {
            return Err(Error::invalid_argument("a rate limit needs capacity > 0"));
        }
        if self.per.is_zero() {
            return Err(Error::invalid_argument("a rate limit needs a window > 0"));
        }
        if self.per > MAX_TTL {
            return Err(Error::invalid_argument(
                "a rate-limit window is at most a year",
            ));
        }
        Ok(())
    }
}

/// The verdict of a rate-limit `take`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    /// The units were taken; `remaining` more could be taken right now.
    Allowed {
        /// Whole units left in the bucket.
        remaining: u64,
    },
    /// Nothing was taken; the same take succeeds after `retry_after` if nobody else takes first.
    Denied {
        /// How long until enough units have refilled.
        retry_after: Duration,
    },
}

/// A cluster node's identity, for second-node detection.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(String);

impl NodeId {
    /// A node id; the name follows the same rules as every coordinator name.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        validate_name(&id)?;
        Ok(Self(id))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
