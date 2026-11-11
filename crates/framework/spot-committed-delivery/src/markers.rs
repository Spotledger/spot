//! The done markers of a committed-steps job.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use spot_app_api::BoxFuture;
use spot_jobs::JobId;

/// A marker store that could not be read or written. The handler answers `Retry`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkerError;

/// Where the handler records which steps of a job are done.
///
/// A production implementation writes the marker to the job's context row; the store's
/// `JobStore` has no such write yet, so the markers are behind this trait.
pub trait StepMarkers: Send + Sync {
    /// The points marked done for `job`, in any order.
    fn done<'a>(&'a self, job: JobId) -> BoxFuture<'a, Result<BTreeSet<String>, MarkerError>>;

    /// Marks `point` done for `job`. Marking twice is harmless.
    fn mark<'a>(&'a self, job: JobId, point: &'a str) -> BoxFuture<'a, Result<(), MarkerError>>;
}

/// Markers kept in memory: for tests, and for a host whose jobs do not outlive the process.
#[derive(Debug, Default)]
pub struct InMemoryMarkers {
    done: Mutex<BTreeMap<u64, BTreeSet<String>>>,
}

impl InMemoryMarkers {
    /// No marker yet.
    pub fn new() -> Self {
        Self::default()
    }
}

impl StepMarkers for InMemoryMarkers {
    fn done<'a>(&'a self, job: JobId) -> BoxFuture<'a, Result<BTreeSet<String>, MarkerError>> {
        Box::pin(async move {
            let guard = self.done.lock().map_err(|_| MarkerError)?;
            Ok(guard.get(&job.0).cloned().unwrap_or_default())
        })
    }

    fn mark<'a>(&'a self, job: JobId, point: &'a str) -> BoxFuture<'a, Result<(), MarkerError>> {
        Box::pin(async move {
            let mut guard = self.done.lock().map_err(|_| MarkerError)?;
            guard.entry(job.0).or_default().insert(point.to_owned());
            Ok(())
        })
    }
}
