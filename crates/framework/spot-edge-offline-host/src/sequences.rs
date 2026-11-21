//! The naming path over the leased ranges: [`SwitchedSequences`].
//!
//! The node's `Namer` allocates numbers through the storage [`Sequences`] trait. On an edge a
//! series configured for leasing (`services.edge_offline.series`) must never be numbered by the
//! site's own transactional counter, which another branch's counter would collide with after the
//! first sync. This wrapper sends such a series to the held lease and every other series to the
//! site's own allocator, unchanged.
//!
//! The decision is by **series, not by connectivity**. The brief names "the lease while offline
//! and the hub allocation while online"; a per-name round trip to the hub is not available in a
//! synchronous allocation and would make the names of one series come from two schemes, so a
//! configured series is numbered from its lease online and offline alike, and the hub's part is
//! the refill (see the task report).
//!
//! # Exhaustion
//!
//! An empty lease fails with `edge_offline.series_exhausted`, naming the series, as
//! [`ErrorKind::Unavailable`] (retryable: a refill is on its way once the hub is reachable). The
//! number is never taken from another source and never reused.

use std::collections::BTreeSet;
use std::future::Future;

use spot_leased_sequences::LeasedSequencesError;
use spot_storage_contract::{ErrorKind, Sequences, SeriesKey, StorageError, StorageResult};

use crate::ranges::SharedSequences;
use crate::series::SeriesExhausted;

/// [`Sequences`] that numbers the configured series from the leased ranges and the rest from
/// `inner`.
pub struct SwitchedSequences<S: Sequences> {
    inner: S,
    leased: SharedSequences,
    leased_series: BTreeSet<String>,
}

impl<S: Sequences> SwitchedSequences<S> {
    /// A switch over `inner` (the site's own allocator) and the shared lease allocator, for the
    /// named series.
    pub fn new(
        inner: S,
        leased: SharedSequences,
        series: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            inner,
            leased,
            leased_series: series.into_iter().collect(),
        }
    }

    /// Whether `series` is numbered from a lease.
    pub fn is_leased(&self, series: &str) -> bool {
        self.leased_series.contains(series)
    }

    fn allocate_leased(&self, series: &SeriesKey, count: u32) -> StorageResult<u64> {
        let mut guard = self.leased.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .allocate_block(series, count)
            .map_err(|error| match error {
                LeasedSequencesError::LeaseExhausted { .. } => StorageError::new(
                    ErrorKind::Unavailable,
                    SeriesExhausted {
                        series: series.as_str().to_owned(),
                    }
                    .to_string(),
                ),
                other => StorageError::from(other),
            })
    }
}

impl<S: Sequences> Sequences for SwitchedSequences<S> {
    fn allocate(
        &mut self,
        series: &SeriesKey,
        count: u32,
    ) -> impl Future<Output = StorageResult<u64>> + Send {
        let local = self
            .is_leased(series.as_str())
            .then(|| self.allocate_leased(series, count));
        let series = series.clone();
        let inner = &mut self.inner;
        async move {
            match local {
                Some(result) => result,
                None => inner.allocate(&series, count).await,
            }
        }
    }

    fn set_next(
        &mut self,
        series: &SeriesKey,
        next: u64,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        let refused = self.is_leased(series.as_str()).then(|| {
            StorageError::from(LeasedSequencesError::SetNextRefused {
                series: series.as_str().to_owned(),
                next,
            })
        });
        let series = series.clone();
        let inner = &mut self.inner;
        async move {
            match refused {
                Some(error) => Err(error),
                None => inner.set_next(&series, next).await,
            }
        }
    }
}
