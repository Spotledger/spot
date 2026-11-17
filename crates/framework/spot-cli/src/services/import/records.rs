//! The job record store of the import service: the seam ([`JobRecords`]), the record types and
//! an in-memory store used by the tests and as the fallback while the platform DocTypes
//! `import_job` and `import_job_error` are not declared (see `import.md`).
//!
//! A record never holds a cell value: row errors keep the row number, the column name and the
//! error code only; the cell text is re-read from the file when a page is served.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use spot_http::api::ports::import::{
    ImportCountsOut, ImportMappingIn, ImportModeIn, ImportStateOut,
};
use spot_http::api::service::{ApiError, BoxFuture};

/// What a store call answers.
pub type RecordResult<T> = Result<T, ApiError>;

/// One import job, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRecord {
    /// The job id (a record id).
    pub id: String,
    /// The site; every lookup carries it.
    pub site: String,
    /// The creator's id: the only user who may see or run the job.
    pub creator: String,
    /// The uploaded file's id.
    pub file_id: String,
    /// The target DocType.
    pub doctype: String,
    /// The mode.
    pub mode: ImportModeIn,
    /// The mapping.
    pub mapping: Option<ImportMappingIn>,
    /// Whether rows are submitted.
    pub submit: bool,
    /// The state.
    pub state: ImportStateOut,
    /// The counts.
    pub counts: ImportCountsOut,
    /// Progress per mille.
    pub progress: u32,
    /// The dry run's digest.
    pub digest: Option<String>,
    /// The digest of the file's bytes, as the files service reports it.
    pub file_digest: Option<String>,
    /// The created-at instant as RFC 3339 text.
    pub created_at: String,
    /// Set by `cancel`; the runner stops after the current batch.
    pub cancel_requested: bool,
    /// The worker that holds the run, and when its lease ends (unix seconds).
    pub lease: Option<(String, i64)>,
    /// The code the job failed with, when `state` is `failed`.
    pub failure: Option<String>,
}

/// One stored row error: no cell text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredError {
    /// The row, from 1.
    pub row: u64,
    /// The column, when the error is about one.
    pub column: Option<String>,
    /// The dotted code.
    pub code: String,
    /// The field the column maps to (for the write-right check when the page is served).
    pub field: Option<String>,
}

/// The durable record store. Every method carries the site; an id of another site is not found.
pub trait JobRecords: Send + Sync {
    /// Inserts a new job.
    fn insert<'a>(&'a self, job: JobRecord) -> BoxFuture<'a, RecordResult<()>>;
    /// One job of `site`.
    fn get<'a>(
        &'a self,
        site: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, RecordResult<Option<JobRecord>>>;
    /// Replaces a job (the caller read it first and changes what it owns).
    fn put<'a>(&'a self, job: JobRecord) -> BoxFuture<'a, RecordResult<()>>;
    /// The jobs of `site` in state `running` whose lease is absent or ended before `now`.
    fn runnable<'a>(
        &'a self,
        site: &'a str,
        now: i64,
    ) -> BoxFuture<'a, RecordResult<Vec<JobRecord>>>;
    /// The number of jobs of `site` in a working state (`dry_running` or `running`).
    fn working<'a>(&'a self, site: &'a str) -> BoxFuture<'a, RecordResult<u64>>;
    /// Replaces the row errors of a job.
    fn put_errors<'a>(
        &'a self,
        job: &'a str,
        errors: Vec<StoredError>,
    ) -> BoxFuture<'a, RecordResult<()>>;
    /// A page of row errors from `offset`.
    fn errors<'a>(
        &'a self,
        job: &'a str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<'a, RecordResult<(Vec<StoredError>, bool)>>;
    /// The job a creator's idempotency key created, with the request fingerprint it was for.
    fn replay<'a>(
        &'a self,
        site: &'a str,
        creator: &'a str,
        operation: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, RecordResult<Option<(String, String)>>>;
    /// Remembers that `key` of `creator` made `job` for `fingerprint`.
    fn remember<'a>(
        &'a self,
        site: &'a str,
        creator: &'a str,
        operation: &'a str,
        key: &'a str,
        fingerprint: String,
        job: String,
    ) -> BoxFuture<'a, RecordResult<()>>;
}

#[derive(Default)]
struct Inner {
    jobs: Vec<JobRecord>,
    errors: HashMap<String, Vec<StoredError>>,
    replays: HashMap<(String, String, String, String), (String, String)>,
}

/// An in-memory [`JobRecords`].
#[derive(Default)]
pub struct MemoryRecords {
    inner: Mutex<Inner>,
}

impl MemoryRecords {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl JobRecords for MemoryRecords {
    fn insert<'a>(&'a self, job: JobRecord) -> BoxFuture<'a, RecordResult<()>> {
        Box::pin(async move {
            self.lock().jobs.push(job);
            Ok(())
        })
    }

    fn get<'a>(
        &'a self,
        site: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, RecordResult<Option<JobRecord>>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .jobs
                .iter()
                .find(|j| j.site == site && j.id == id)
                .cloned())
        })
    }

    fn put<'a>(&'a self, job: JobRecord) -> BoxFuture<'a, RecordResult<()>> {
        Box::pin(async move {
            let mut inner = self.lock();
            if let Some(slot) = inner
                .jobs
                .iter_mut()
                .find(|j| j.site == job.site && j.id == job.id)
            {
                *slot = job;
            }
            Ok(())
        })
    }

    fn runnable<'a>(
        &'a self,
        site: &'a str,
        now: i64,
    ) -> BoxFuture<'a, RecordResult<Vec<JobRecord>>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .jobs
                .iter()
                .filter(|j| {
                    j.site == site
                        && j.state == ImportStateOut::Running
                        && j.lease.as_ref().is_none_or(|(_, until)| *until <= now)
                })
                .cloned()
                .collect())
        })
    }

    fn working<'a>(&'a self, site: &'a str) -> BoxFuture<'a, RecordResult<u64>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .jobs
                .iter()
                .filter(|j| {
                    j.site == site
                        && matches!(
                            j.state,
                            ImportStateOut::DryRunning | ImportStateOut::Running
                        )
                })
                .count() as u64)
        })
    }

    fn put_errors<'a>(
        &'a self,
        job: &'a str,
        errors: Vec<StoredError>,
    ) -> BoxFuture<'a, RecordResult<()>> {
        Box::pin(async move {
            self.lock().errors.insert(job.to_owned(), errors);
            Ok(())
        })
    }

    fn errors<'a>(
        &'a self,
        job: &'a str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<'a, RecordResult<(Vec<StoredError>, bool)>> {
        Box::pin(async move {
            let inner = self.lock();
            let all = inner.errors.get(job).map(Vec::as_slice).unwrap_or_default();
            let end = offset.saturating_add(limit).min(all.len());
            let page = all.get(offset..end).unwrap_or_default().to_vec();
            Ok((page, end < all.len()))
        })
    }

    fn replay<'a>(
        &'a self,
        site: &'a str,
        creator: &'a str,
        operation: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, RecordResult<Option<(String, String)>>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .replays
                .get(&(
                    site.to_owned(),
                    creator.to_owned(),
                    operation.to_owned(),
                    key.to_owned(),
                ))
                .cloned())
        })
    }

    fn remember<'a>(
        &'a self,
        site: &'a str,
        creator: &'a str,
        operation: &'a str,
        key: &'a str,
        fingerprint: String,
        job: String,
    ) -> BoxFuture<'a, RecordResult<()>> {
        Box::pin(async move {
            self.lock().replays.insert(
                (
                    site.to_owned(),
                    creator.to_owned(),
                    operation.to_owned(),
                    key.to_owned(),
                ),
                (fingerprint, job),
            );
            Ok(())
        })
    }
}
