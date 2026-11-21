//! The done markers of a committed-steps job.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use spot_app_api::BoxFuture;
use spot_jobs::model::Claim;
use spot_jobs::{JobId, JobStore};

/// A marker store that could not be read or written. The handler answers `Retry`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkerError;

/// Where the handler records which steps of a job are done.
///
/// The production implementation, [`JobStoreMarkers`], writes the marker to the job's context
/// row through the claim of the running attempt; [`InMemoryMarkers`] is for tests.
pub trait StepMarkers: Send + Sync {
    /// The points marked done for `job`, in any order.
    fn done<'a>(&'a self, job: JobId) -> BoxFuture<'a, Result<BTreeSet<String>, MarkerError>>;

    /// Marks `point` done for `job`, as the attempt that holds `claim`. Marking twice is
    /// harmless. A store-backed implementation refuses (`MarkerError`) without a claim or with
    /// a lost one, so a worker that lost its lease can never record progress.
    fn mark<'a>(
        &'a self,
        job: JobId,
        claim: Option<&'a Claim>,
        point: &'a str,
    ) -> BoxFuture<'a, Result<(), MarkerError>>;
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

    fn mark<'a>(
        &'a self,
        job: JobId,
        _claim: Option<&'a Claim>,
        point: &'a str,
    ) -> BoxFuture<'a, Result<(), MarkerError>> {
        Box::pin(async move {
            let mut guard = self.done.lock().map_err(|_| MarkerError)?;
            guard.entry(job.0).or_default().insert(point.to_owned());
            Ok(())
        })
    }
}

/// The context key that holds the done points: a JSON array of point names.
pub const CONTEXT_KEY: &str = "committed_done";

/// Markers kept in the job's own context row, so they survive a crash and a restart.
///
/// `done` reads the context key [`CONTEXT_KEY`] of the stored job; `mark` merges the point into
/// that array with `JobStore::merge_context`, fenced by the claim of the running attempt. Every
/// failure (a store error, a missing job, a lost claim, a context that is not a list of names)
/// is a [`MarkerError`]: the handler then answers `Retry` and repeats nothing it has not marked.
pub struct JobStoreMarkers {
    store: Arc<dyn JobStore>,
}

impl JobStoreMarkers {
    /// Markers over `store`, which must be the store the worker claims from.
    pub fn new(store: Arc<dyn JobStore>) -> Self {
        Self { store }
    }

    async fn read(&self, job: JobId) -> Result<BTreeSet<String>, MarkerError> {
        let stored = self
            .store
            .get(job)
            .await
            .map_err(|_| MarkerError)?
            .ok_or(MarkerError)?;
        match stored.context.get(CONTEXT_KEY) {
            None | Some(serde_json::Value::Null) => Ok(BTreeSet::new()),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| item.as_str().map(str::to_owned).ok_or(MarkerError))
                .collect(),
            // A corrupt value is never read as "nothing done": that would repeat finished steps.
            Some(_) => Err(MarkerError),
        }
    }
}

impl StepMarkers for JobStoreMarkers {
    fn done<'a>(&'a self, job: JobId) -> BoxFuture<'a, Result<BTreeSet<String>, MarkerError>> {
        Box::pin(self.read(job))
    }

    fn mark<'a>(
        &'a self,
        job: JobId,
        claim: Option<&'a Claim>,
        point: &'a str,
    ) -> BoxFuture<'a, Result<(), MarkerError>> {
        Box::pin(async move {
            let claim = claim.ok_or(MarkerError)?;
            let mut done = self.read(job).await?;
            done.insert(point.to_owned());
            let list = done.into_iter().map(serde_json::Value::String).collect();
            let mut patch = serde_json::Map::new();
            patch.insert(CONTEXT_KEY.to_owned(), serde_json::Value::Array(list));
            self.store
                .merge_context(job, claim, patch)
                .await
                .map(|_| ())
                .map_err(|_| MarkerError)
        })
    }
}
