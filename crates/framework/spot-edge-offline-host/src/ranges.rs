//! The ranges an edge holds: the book over the leased series, the request id rule and the
//! installation of a granted range.
//!
//! The pieces are the library's: [`LeasedSequences`] over [`BranchSeries`] keeps the numbers,
//! a [`HighWaterStore`] keeps them durable (see [`crate::store_pg`]). This module is the host's
//! glue and decides nothing the library already decides; it adds:
//!
//! * **Restart**: [`RangeBook::open`] reopens every configured series from the store, so a
//!   reboot continues at the stored high-water mark and never reissues a number.
//! * **Idempotent re-request**: the request id of a lease is `<series>@<last number held>`. A
//!   request resent after a lost answer, or after a crash between the hub's grant and the
//!   install, carries the same id, so the hub (which answers a repeated id with the original
//!   range) returns the same range and it is installed once. After an install the held range
//!   moves up, so the next request has a new id.
//! * **Fail closed**: an answer for a series that is not configured, for another node, or that
//!   would not lie strictly above the held range is refused and installs nothing.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_leased_sequences::{LeasedSequences, LeasedSequencesError};
use spot_partition::{BranchSeries, HighWaterStore, LeaseId, LowWater, PartitionError, RangeLease};
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;

use crate::carrier::{LeaseAnswerMsg, LeaseRequestMsg};
use crate::series::{RangeView, SeriesPolicy};

/// The stable code of a range operation that failed.
pub const RANGE_REFUSED: &str = "edge_offline.range_refused";
/// The stable code of a store failure on the ranges.
pub const RANGE_STORE: &str = "edge_offline.range_store";

/// A range operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeError {
    /// The stable code ([`RANGE_REFUSED`] or [`RANGE_STORE`]).
    pub code: &'static str,
    /// A message with the series name and a reason; never a number of another node.
    pub message: String,
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RangeError {}

fn refused(message: impl Into<String>) -> RangeError {
    RangeError {
        code: RANGE_REFUSED,
        message: message.into(),
    }
}

fn store_failed(message: impl Into<String>) -> RangeError {
    RangeError {
        code: RANGE_STORE,
        message: message.into(),
    }
}

/// What installing an answer did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installed {
    /// A new range was installed.
    New,
    /// The same range was already held (a duplicate answer): nothing changed.
    AlreadyHeld,
}

/// The shared allocator: one per edge, behind a lock the naming path and the carrier share. The
/// allocation holds the lock only for a synchronous call (no await while it is held).
pub type SharedSequences = Arc<Mutex<LeasedSequences>>;

/// The ranges one edge holds for its configured series.
pub struct RangeBook {
    node: NodeId,
    series: Vec<SeriesPolicy>,
    sequences: SharedSequences,
    store: Arc<dyn HighWaterStore>,
    low: LowWater,
}

impl fmt::Debug for RangeBook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RangeBook")
            .field("node", &self.node)
            .field("series", &self.series)
            .finish_non_exhaustive()
    }
}

impl RangeBook {
    /// Opens the book: every configured series is reopened from `store` (a series the store has
    /// no state for simply holds no range yet).
    ///
    /// # Errors
    ///
    /// [`RANGE_REFUSED`] for a series name that is not a legal key, and [`RANGE_STORE`] when the
    /// stored state of a series is unusable (fail closed: the series stays unusable rather than
    /// restarting from an unknown mark).
    pub fn open(
        node: NodeId,
        series: Vec<SeriesPolicy>,
        store: Arc<dyn HighWaterStore>,
        low: LowWater,
    ) -> Result<Self, RangeError> {
        let mut sequences = LeasedSequences::new();
        for policy in &series {
            let key = key_of(&policy.name)?;
            match BranchSeries::reopen(&key, &node, low, Arc::clone(&store)) {
                Ok(branch) => {
                    sequences
                        .install(branch)
                        .map_err(|error| store_failed(format!("{}: {error}", policy.name)))?;
                }
                Err(PartitionError::NoLease { .. }) => {}
                Err(error) => {
                    return Err(store_failed(format!(
                        "the stored range of series {} is unusable ({})",
                        policy.name,
                        error.code()
                    )));
                }
            }
        }
        Ok(Self {
            node,
            series,
            sequences: Arc::new(Mutex::new(sequences)),
            store,
            low,
        })
    }

    /// The shared allocator the naming path uses.
    pub fn sequences(&self) -> SharedSequences {
        Arc::clone(&self.sequences)
    }

    /// The configured series.
    pub fn series(&self) -> &[SeriesPolicy] {
        &self.series
    }

    /// The node the ranges are leased to.
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    fn guard(&self) -> MutexGuard<'_, LeasedSequences> {
        self.sequences.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The last number of the range held for `series`, or 0 when none is held.
    pub fn last_held(&self, series: &str) -> u64 {
        let Ok(key) = key_of(series) else { return 0 };
        self.guard().branch(&key).map_or(0, |b| b.lease().to)
    }

    /// The lease request for `series`, with the idempotent id; `None` for a series that is not
    /// configured.
    pub fn request_for(&self, series: &str) -> Option<LeaseRequestMsg> {
        let policy = self.series.iter().find(|p| p.name == series)?;
        Some(LeaseRequestMsg {
            request_id: request_id(series, self.last_held(series)),
            series: policy.name.clone(),
            size: policy.block,
        })
    }

    /// Installs a hub answer.
    ///
    /// A refusal from the hub installs nothing and is not an error here: the carrier loop reads
    /// the code and backs off.
    ///
    /// # Errors
    ///
    /// [`RANGE_REFUSED`] for an unconfigured series, an invalid range or a range that does not
    /// lie strictly above the held one (the held range is untouched), and [`RANGE_STORE`] when
    /// the high-water mark cannot be written (nothing is installed, so the same answer may be
    /// installed again).
    pub fn install(&self, answer: &LeaseAnswerMsg) -> Result<Installed, RangeError> {
        let LeaseAnswerMsg::Granted {
            series,
            from,
            to,
            lease_id,
            ..
        } = answer
        else {
            return Err(refused("a refusal carries no range"));
        };
        if !self.series.iter().any(|p| &p.name == series) {
            return Err(refused(format!("series {series} is not configured")));
        }
        let key = key_of(series)?;
        let lease = RangeLease::new(
            key.clone(),
            *from,
            *to,
            self.node.clone(),
            LeaseId::new(*lease_id),
        )
        .map_err(|_| refused(format!("the range granted for series {series} is invalid")))?;
        let mut sequences = self.guard();
        if sequences.branch(&key).is_some_and(|b| b.lease() == &lease) {
            return Ok(Installed::AlreadyHeld);
        }
        sequences
            .renew(lease, self.low, Arc::clone(&self.store))
            .map_err(|error| match error {
                LeasedSequencesError::Store { .. } => {
                    store_failed(format!("the range of series {series} was not stored"))
                }
                _ => refused(format!(
                    "the range granted for series {series} does not lie above the held range"
                )),
            })?;
        Ok(Installed::New)
    }

    /// Numbers left per series, in configuration order (for the readiness line).
    pub fn remaining_all(&self) -> Vec<(String, u64)> {
        self.series
            .iter()
            .map(|p| (p.name.clone(), self.remaining(&p.name)))
            .collect()
    }

    /// The series whose range is exhausted (nothing left, or none held).
    pub fn exhausted(&self) -> Vec<String> {
        self.series
            .iter()
            .filter(|p| self.remaining(&p.name) == 0)
            .map(|p| p.name.clone())
            .collect()
    }

    /// The remaining numbers of every series in a map, for a view that needs ownership.
    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        self.remaining_all().into_iter().collect()
    }
}

impl RangeView for RangeBook {
    fn remaining(&self, series: &str) -> u64 {
        let Ok(key) = key_of(series) else { return 0 };
        self.guard().remaining(&key)
    }
}

/// The partition crate's node id for a configured node name.
///
/// # Errors
///
/// [`RANGE_REFUSED`] when the name is not a legal node id.
pub fn partition_node(name: &str) -> Result<NodeId, RangeError> {
    NodeId::new(name).map_err(|_| refused("the partition node name is not a legal node id"))
}

/// The idempotent request id of a lease for `series` after the number `last_held`.
pub fn request_id(series: &str, last_held: u64) -> String {
    format!("lease:{series}@{last_held}")
}

fn key_of(series: &str) -> Result<SeriesKey, RangeError> {
    SeriesKey::new(series).map_err(|_| refused(format!("series {series} is not a legal key")))
}
