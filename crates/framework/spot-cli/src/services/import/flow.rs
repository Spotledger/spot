//! The import service's logic: the two-phase state machine, limits, digest, creator-only access,
//! idempotent create and run, audit, and the durable runner. Everything host specific (who the
//! caller is, the import right, reading the uploaded file, the import engine) is behind
//! [`ImportHost`], so this file is tested without a database (`tests/import_service.rs`).
//!
//! # State
//!
//! `create` stages the file, runs the dry run and leaves the job `ready` with its digest (the
//! contract allows `dry_running`; this service finishes the dry run before it answers, so the
//! caller never polls for it). `run` needs `ready` and the digest; it re-checks the rights,
//! marks the job `running` with no lease and answers; the runner ([`ImportCore::pump`]) takes
//! it from there in bounded batches. A job whose lease ended (a crash) is `running` with a
//! stale lease and is taken again; the engine's per-row idempotency (the batch label is the job
//! id) skips rows already written, so the final counts are exact and no row is written twice.
//!
//! # What is never stored or logged
//!
//! Cell text, tokens and file names. A row error keeps the row, the column name and the code.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use spot_core::Clock;
use spot_http::api::ports::import::{
    ImportCountsOut, ImportCreateIn, ImportErrorsPageOut, ImportJobOut, ImportLimitsOut,
    ImportModeIn, ImportPort, ImportResult, ImportRowErrorOut, ImportStateOut, MAX_CELL_TEXT,
    MAX_ERRORS_PAGE,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use time::format_description::well_known::Rfc3339;

use super::records::{JobRecord, JobRecords, StoredError};

/// The limits and batch sizes of `[services.import]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The largest file in bytes.
    pub max_file_bytes: u64,
    /// The most rows.
    pub max_rows: u64,
    /// The most columns.
    pub max_columns: u64,
    /// The most dry runs and runs working at once on the site.
    pub max_concurrent_per_site: u64,
    /// Rows per batch (progress, cancel and the lease are checked between batches).
    pub batch_rows: u64,
    /// Failed rows after which a run stops.
    pub max_errors: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: 50 * 1024 * 1024,
            max_rows: 100_000,
            max_columns: 200,
            max_concurrent_per_site: 2,
            batch_rows: 500,
            max_errors: 100,
        }
    }
}

/// The authenticated caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The user's record id.
    pub user: String,
}

/// An uploaded file read out of the files service into a private spool file owned by a job.
/// The file is removed when a value that `owned` it is dropped.
#[derive(Debug)]
pub struct StagedFile {
    /// Where the bytes are.
    pub path: PathBuf,
    /// Their size.
    pub bytes: u64,
    /// The digest the files service reports for them, as text.
    pub digest: String,
    /// Whether this value owns (and removes) the file.
    pub owned: bool,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Why the host refused a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The caller lacks the import (or submit) right, or is no longer an active user.
    RightMissing,
    /// The file or job is not the caller's, or does not exist.
    NotFound,
    /// A limit was exceeded (bytes, rows, columns).
    LimitExceeded,
    /// The file cannot be read as a table, or has failed its scan.
    FileUnreadable,
    /// The mapping names a column or field that cannot be used.
    MappingInvalid,
    /// The service is not ready (missing DocTypes, no files service).
    Unavailable,
}

impl Refusal {
    /// The registry code of this refusal.
    pub fn code(self) -> Code {
        match self {
            Refusal::RightMissing => Code::ImportRightMissing,
            Refusal::NotFound => Code::DocumentNotFound,
            Refusal::LimitExceeded => Code::ImportLimitExceeded,
            Refusal::FileUnreadable => Code::ImportFileUnreadable,
            Refusal::MappingInvalid => Code::ImportMappingInvalid,
            Refusal::Unavailable => Code::ServerUnavailable,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Refusal::RightMissing => "right_missing",
            Refusal::NotFound => "not_found",
            Refusal::LimitExceeded => "limit_exceeded",
            Refusal::FileUnreadable => "file_unreadable",
            Refusal::MappingInvalid => "mapping_invalid",
            Refusal::Unavailable => "unavailable",
        }
    }
}

/// What the engine did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineOutcome {
    /// Rows read.
    pub rows: u64,
    /// Rows written (or that would be).
    pub ok: u64,
    /// Rows left alone.
    pub skipped: u64,
    /// The row errors, without cell text.
    pub errors: Vec<StoredError>,
    /// Whether the run stopped on the cancel flag.
    pub cancelled: bool,
}

/// What the engine needs of one job.
pub struct EngineRequest<'a> {
    /// The site.
    pub site: &'a str,
    /// The creator.
    pub user: &'a str,
    /// The job.
    pub job: &'a JobRecord,
    /// The staged file.
    pub file: &'a StagedFile,
    /// The limits.
    pub limits: &'a Limits,
}

/// The run's side channel: progress out, cancel in.
pub trait RunControl: Send + Sync {
    /// `done` rows are finished (`total` when known).
    fn progress(&self, done: u64, total: Option<u64>);
    /// Whether the caller asked to stop.
    fn cancelled(&self) -> bool;
}

/// Everything host specific. The production implementation is `host.rs`.
pub trait ImportHost: Send + Sync {
    /// Who the request's credential names; `auth.required` and the like otherwise.
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<Caller, ApiError>>;
    /// Whether `user` is still an active user of the site (re-resolved at run time).
    fn active<'a>(&'a self, site: &'a str, user: &'a str) -> BoxFuture<'a, bool>;
    /// The import right on `doctype` (and the submit right when `submit`), as of now.
    fn may_import<'a>(
        &'a self,
        site: &'a str,
        user: &'a str,
        doctype: &'a str,
        submit: bool,
    ) -> BoxFuture<'a, Result<(), Refusal>>;
    /// Reads the file `file_id` (as the caller the request names: its tenant, its scan state)
    /// into a private spool file owned by the job `job_id`, refusing one that is not the
    /// caller's tenant's, not clean after its scan, or over `max_bytes`. The spool file stays
    /// until [`ImportHost::discard`], so a run after a restart reads the same bytes the dry run
    /// did.
    fn stage<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        file_id: &'a str,
        job_id: &'a str,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<StagedFile, Refusal>>;
    /// The spool file of a job, if it is still there.
    fn spool<'a>(&'a self, job_id: &'a str) -> BoxFuture<'a, Option<StagedFile>>;
    /// Removes a job's spool file (the job is finished, cancelled or failed).
    fn discard<'a>(&'a self, job_id: &'a str) -> BoxFuture<'a, ()>;
    /// The dry run: reads, maps, converts and validates every row, writes nothing.
    fn dry_run<'a>(
        &'a self,
        request: &'a EngineRequest<'a>,
    ) -> BoxFuture<'a, Result<EngineOutcome, Refusal>>;
    /// The run: rows through the lifecycle as `request.user`, in batches, idempotent per row.
    fn run<'a>(
        &'a self,
        request: &'a EngineRequest<'a>,
        control: &'a dyn RunControl,
    ) -> BoxFuture<'a, Result<EngineOutcome, Refusal>>;
    /// The cell text of `(row, column)` pairs for an errors page, only for columns whose field
    /// the caller may write; others are absent from the answer.
    fn cells<'a>(
        &'a self,
        site: &'a str,
        user: &'a str,
        job: &'a JobRecord,
        wanted: &'a [(u64, String)],
    ) -> BoxFuture<'a, Vec<(u64, String, String)>>;
    /// The metadata version the dry run validates against (an input to the digest).
    fn meta_version(&self) -> u64;
}

/// What an audit record says: who, what, which job, the outcome and the rule. Never a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    /// The site.
    pub site: String,
    /// The user.
    pub user: String,
    /// The job, when there is one.
    pub job: Option<String>,
    /// The DocType, when known.
    pub doctype: Option<String>,
    /// `created`, `dry_run`, `run_started`, `finished`, `cancelled` or `refused`.
    pub action: &'static str,
    /// `ok`, `failed` or the refusal's word.
    pub outcome: &'static str,
}

/// Where audit records go.
pub trait ImportAudit: Send + Sync {
    /// Records one event.
    fn record(&self, event: &AuditEvent);
}

struct Ctl {
    cancel: AtomicBool,
    done: AtomicU64,
    total: AtomicU64,
}

impl RunControl for Ctl {
    fn progress(&self, done: u64, total: Option<u64>) {
        self.done.store(done, Ordering::Relaxed);
        if let Some(total) = total {
            self.total.store(total, Ordering::Relaxed);
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// The service core: implements [`ImportPort`] and runs the jobs.
pub struct ImportCore {
    site: String,
    host: Arc<dyn ImportHost>,
    records: Arc<dyn JobRecords>,
    audit: Arc<dyn ImportAudit>,
    clock: Arc<dyn Clock>,
    limits: Limits,
    worker: String,
}

/// How long a run's lease lasts without a heartbeat, in seconds.
pub const LEASE_SECONDS: i64 = 60;

impl ImportCore {
    /// A core for `site`.
    pub fn new(
        site: impl Into<String>,
        host: Arc<dyn ImportHost>,
        records: Arc<dyn JobRecords>,
        audit: Arc<dyn ImportAudit>,
        clock: Arc<dyn Clock>,
        limits: Limits,
        worker: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(ImportCore {
            site: site.into(),
            host,
            records,
            audit,
            clock,
            limits,
            worker: worker.into(),
        })
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn event(
        &self,
        user: &str,
        job: Option<&JobRecord>,
        action: &'static str,
        outcome: &'static str,
    ) {
        self.audit.record(&AuditEvent {
            site: self.site.clone(),
            user: user.to_owned(),
            job: job.map(|j| j.id.clone()),
            doctype: job.map(|j| j.doctype.clone()),
            action,
            outcome,
        });
    }

    fn refuse(&self, user: &str, job: Option<&JobRecord>, refusal: Refusal) -> ApiError {
        self.event(user, job, "refused", refusal.word());
        ApiError::from_code(refusal.code())
    }

    fn out(job: &JobRecord) -> ImportJobOut {
        ImportJobOut {
            id: job.id.clone(),
            doctype: job.doctype.clone(),
            mode: job.mode,
            state: job.state,
            mapping: job.mapping.clone(),
            submit: Some(job.submit),
            counts: job.counts,
            progress: job.progress.min(1000),
            digest: job.digest.clone(),
            created_by: job.creator.clone(),
            created_at: job.created_at.clone(),
        }
    }

    /// The creator's own job, or the one `404` of a job that is not theirs.
    async fn own(&self, caller: &Caller, id: &str) -> ImportResult<JobRecord> {
        match self.records.get(&self.site, id).await? {
            Some(job) if job.creator == caller.user => Ok(job),
            _ => Err(ApiError::from_code(Code::DocumentNotFound)),
        }
    }

    fn digest(&self, job: &JobRecord, file_digest: &str) -> String {
        let mut hasher = Sha256::new();
        let mut part = |text: &str| {
            hasher.update(text.len().to_le_bytes());
            hasher.update(text.as_bytes());
        };
        part(file_digest);
        part(&job.doctype);
        part(job.mode.as_str());
        part(if job.submit { "submit" } else { "save" });
        part(&self.host.meta_version().to_string());
        if let Some(mapping) = &job.mapping {
            for (column, field) in &mapping.columns {
                part(column);
                part(field);
            }
            for (table, prefix) in &mapping.child_tables {
                part(table);
                part(prefix);
            }
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn fingerprint(request: &ImportCreateIn) -> String {
        format!("{request:?}")
    }

    async fn create_job(
        &self,
        ctx: &RequestCtx,
        request: ImportCreateIn,
    ) -> ImportResult<ImportJobOut> {
        let caller = self.host.caller(ctx).await?;
        let user = caller.user.clone();
        let fingerprint = Self::fingerprint(&request);
        if let Some(key) = &ctx.idempotency_key
            && let Some((seen, job)) = self
                .records
                .replay(&self.site, &user, "create", key)
                .await?
        {
            return if seen == fingerprint {
                match self.records.get(&self.site, &job).await? {
                    Some(job) => Ok(Self::out(&job)),
                    None => Err(ApiError::from_code(Code::DocumentNotFound)),
                }
            } else {
                Err(ApiError::from_code(Code::ApiIdempotencyConflict))
            };
        }
        if let Err(refusal) = self
            .host
            .may_import(&self.site, &user, &request.doctype, request.submit)
            .await
        {
            return Err(self.refuse(&user, None, refusal));
        }
        if !request.dry_run {
            self.event(&user, None, "refused", "not_ready");
            return Err(ApiError::new(
                Code::ImportNotReady,
                "A run needs the digest of a dry run.",
            ));
        }
        if self.records.working(&self.site).await? >= self.limits.max_concurrent_per_site {
            return Err(self.refuse(&user, None, Refusal::LimitExceeded));
        }
        // The job id is a record id: it names the spool file and, later, the job in the store.
        let job_id = spot_app_api::prelude::RecordId::new_v7().to_string();
        let staged = match self
            .host
            .stage(ctx, &request.file_id, &job_id, self.limits.max_file_bytes)
            .await
        {
            Ok(staged) => staged,
            Err(refusal) => return Err(self.refuse(&user, None, refusal)),
        };
        let created_at = self.clock.now_utc().format(&Rfc3339).unwrap_or_default();
        let mut job = JobRecord {
            id: job_id.clone(),
            site: self.site.clone(),
            creator: user.clone(),
            file_id: request.file_id.clone(),
            doctype: request.doctype.clone(),
            mode: request.mode,
            mapping: request.mapping.clone(),
            submit: request.submit,
            state: ImportStateOut::DryRunning,
            counts: ImportCountsOut::default(),
            progress: 0,
            digest: None,
            file_digest: Some(staged.digest.clone()),
            created_at,
            cancel_requested: false,
            lease: None,
            failure: None,
        };
        self.records.insert(job.clone()).await?;
        self.event(&user, Some(&job), "created", "ok");
        let outcome = {
            let engine = EngineRequest {
                site: &self.site,
                user: &user,
                job: &job,
                file: &staged,
                limits: &self.limits,
            };
            self.host.dry_run(&engine).await
        };
        match outcome {
            Ok(done) => {
                job.counts = ImportCountsOut {
                    rows: done.rows,
                    ok: done.ok,
                    errors: done.errors.len() as u64,
                    skipped: done.skipped,
                };
                job.digest = Some(self.digest(&job, &staged.digest));
                job.state = ImportStateOut::Ready;
                self.records.put_errors(&job.id, done.errors).await?;
                self.records.put(job.clone()).await?;
                self.event(&user, Some(&job), "dry_run", "ok");
            }
            Err(refusal) => {
                job.state = ImportStateOut::Failed;
                job.failure = Some(refusal.word().to_owned());
                self.records.put(job.clone()).await?;
                self.host.discard(&job.id).await;
                self.event(&user, Some(&job), "refused", refusal.word());
                return Err(ApiError::from_code(refusal.code()));
            }
        }
        if let Some(key) = &ctx.idempotency_key {
            self.records
                .remember(
                    &self.site,
                    &user,
                    "create",
                    key,
                    fingerprint,
                    job.id.clone(),
                )
                .await?;
        }
        Ok(Self::out(&job))
    }

    async fn run_job(
        &self,
        ctx: &RequestCtx,
        id: &str,
        digest: &str,
    ) -> ImportResult<ImportJobOut> {
        let caller = self.host.caller(ctx).await?;
        let user = caller.user.clone();
        let fingerprint = format!("{id}:{digest}");
        if let Some(key) = &ctx.idempotency_key
            && let Some((seen, job)) = self.records.replay(&self.site, &user, "run", key).await?
        {
            return if seen == fingerprint {
                match self.records.get(&self.site, &job).await? {
                    Some(job) => Ok(Self::out(&job)),
                    None => Err(ApiError::from_code(Code::DocumentNotFound)),
                }
            } else {
                Err(ApiError::from_code(Code::ApiIdempotencyConflict))
            };
        }
        let mut job = self.own(&caller, id).await?;
        match job.state {
            ImportStateOut::Ready => {}
            ImportStateOut::Cancelled => {
                self.event(&user, Some(&job), "refused", "cancelled");
                return Err(ApiError::from_code(Code::ImportCancelled));
            }
            _ => {
                self.event(&user, Some(&job), "refused", "not_ready");
                return Err(ApiError::from_code(Code::ImportNotReady));
            }
        }
        if job.digest.as_deref() != Some(digest) {
            self.event(&user, Some(&job), "refused", "digest_changed");
            return Err(ApiError::from_code(Code::ImportDigestChanged));
        }
        // The rights are checked again now: the import right and, for `submit`, the submit right.
        if let Err(refusal) = self
            .host
            .may_import(&self.site, &user, &job.doctype, job.submit)
            .await
        {
            return Err(self.refuse(&user, Some(&job), refusal));
        }
        if self.records.working(&self.site).await? >= self.limits.max_concurrent_per_site {
            return Err(self.refuse(&user, Some(&job), Refusal::LimitExceeded));
        }
        job.state = ImportStateOut::Running;
        job.progress = 0;
        job.lease = None;
        self.records.put(job.clone()).await?;
        self.event(&user, Some(&job), "run_started", "ok");
        if let Some(key) = &ctx.idempotency_key {
            self.records
                .remember(&self.site, &user, "run", key, fingerprint, job.id.clone())
                .await?;
        }
        Ok(Self::out(&job))
    }

    /// Takes every runnable job (a `running` job with no lease or an ended one) and runs it.
    /// Called by the service loop; safe to call concurrently.
    pub async fn pump(self: &Arc<Self>) -> usize {
        let now = self.now();
        let Ok(jobs) = self.records.runnable(&self.site, now).await else {
            return 0;
        };
        let mut started = 0;
        for mut job in jobs {
            job.lease = Some((self.worker.clone(), now + LEASE_SECONDS));
            if self.records.put(job.clone()).await.is_err() {
                continue;
            }
            started += 1;
            let core = Arc::clone(self);
            tokio::spawn(async move { core.execute(job).await });
        }
        started
    }

    /// Runs one claimed job to its end.
    async fn execute(self: Arc<Self>, mut job: JobRecord) {
        let user = job.creator.clone();
        // The creator is resolved again now: a revoked or disabled user's import stops.
        if !self.host.active(&self.site, &user).await {
            return self.fail(job, Refusal::RightMissing).await;
        }
        if let Err(refusal) = self
            .host
            .may_import(&self.site, &user, &job.doctype, job.submit)
            .await
        {
            return self.fail(job, refusal).await;
        }
        let Some(staged) = self.host.spool(&job.id).await else {
            return self.fail(job, Refusal::FileUnreadable).await;
        };
        // A host whose spool copy carries no digest of its own (the copy is private to the job)
        // leaves it empty; one that reports it must match what the dry run saw.
        if !staged.digest.is_empty() && job.file_digest.as_deref() != Some(staged.digest.as_str()) {
            self.event(&user, Some(&job), "refused", "digest_changed");
            job.state = ImportStateOut::Failed;
            job.failure = Some("digest_changed".to_owned());
            job.lease = None;
            self.host.discard(&job.id).await;
            let _ = self.records.put(job).await;
            return;
        }
        let ctl = Arc::new(Ctl {
            cancel: AtomicBool::new(job.cancel_requested),
            done: AtomicU64::new(0),
            total: AtomicU64::new(job.counts.rows),
        });
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let ticker = {
            let core = Arc::clone(&self);
            let ctl = Arc::clone(&ctl);
            let id = job.id.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stopped.changed() => break,
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    }
                    let Ok(Some(mut current)) = core.records.get(&core.site, &id).await else {
                        continue;
                    };
                    if current.cancel_requested {
                        ctl.cancel.store(true, Ordering::Relaxed);
                    }
                    let total = ctl.total.load(Ordering::Relaxed).max(1);
                    let done = ctl.done.load(Ordering::Relaxed);
                    current.progress =
                        u32::try_from((done.saturating_mul(1000) / total).min(999)).unwrap_or(999);
                    current.lease = Some((core.worker.clone(), core.now() + LEASE_SECONDS));
                    let _ = core.records.put(current).await;
                }
            })
        };
        let result = {
            let engine = EngineRequest {
                site: &self.site,
                user: &user,
                job: &job,
                file: &staged,
                limits: &self.limits,
            };
            self.host.run(&engine, ctl.as_ref()).await
        };
        let _ = stop.send(true);
        let _ = ticker.await;
        // Read the job again: `cancel` may have changed it while the run was working.
        let mut current = match self.records.get(&self.site, &job.id).await {
            Ok(Some(current)) => current,
            _ => job.clone(),
        };
        match result {
            Ok(done) => {
                current.counts = ImportCountsOut {
                    rows: done.rows,
                    ok: done.ok,
                    errors: done.errors.len() as u64,
                    skipped: done.skipped,
                };
                let _ = self.records.put_errors(&current.id, done.errors).await;
                if done.cancelled || current.cancel_requested {
                    current.state = ImportStateOut::Cancelled;
                    self.event(&user, Some(&current), "cancelled", "ok");
                } else {
                    current.state = ImportStateOut::Done;
                    current.progress = 1000;
                    self.event(&user, Some(&current), "finished", "ok");
                }
            }
            Err(refusal) => {
                current.state = ImportStateOut::Failed;
                current.failure = Some(refusal.word().to_owned());
                self.event(&user, Some(&current), "finished", "failed");
            }
        }
        current.lease = None;
        self.host.discard(&current.id).await;
        let _ = self.records.put(current).await;
    }

    async fn fail(&self, mut job: JobRecord, refusal: Refusal) {
        self.event(&job.creator, Some(&job), "refused", refusal.word());
        job.state = ImportStateOut::Failed;
        job.failure = Some(refusal.word().to_owned());
        job.lease = None;
        self.host.discard(&job.id).await;
        let _ = self.records.put(job).await;
    }
}

impl ImportPort for ImportCore {
    fn limits<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, ImportResult<ImportLimitsOut>> {
        Box::pin(async move {
            self.host.caller(ctx).await?;
            Ok(ImportLimitsOut {
                max_rows: self.limits.max_rows,
                max_bytes: self.limits.max_file_bytes,
                max_columns: self.limits.max_columns,
                modes: vec![
                    ImportModeIn::Insert,
                    ImportModeIn::Update,
                    ImportModeIn::Upsert,
                ],
            })
        })
    }

    fn create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: ImportCreateIn,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>> {
        Box::pin(self.create_job(ctx, request))
    }

    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>> {
        Box::pin(async move {
            let caller = self.host.caller(ctx).await?;
            Ok(Self::out(&self.own(&caller, id).await?))
        })
    }

    fn errors<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, ImportResult<ImportErrorsPageOut>> {
        Box::pin(async move {
            let caller = self.host.caller(ctx).await?;
            let job = self.own(&caller, id).await?;
            let offset = match cursor {
                None => 0usize,
                Some(text) => text
                    .strip_prefix('c')
                    .and_then(|n| n.parse::<usize>().ok())
                    .ok_or_else(|| {
                        ApiError::new(
                            Code::ApiInvalidRequest,
                            "The cursor is not one this server issued.",
                        )
                    })?,
            };
            let limit = limit.clamp(1, MAX_ERRORS_PAGE) as usize;
            let (page, more) = self.records.errors(&job.id, offset, limit).await?;
            // The cell text is read from the file again, only for fields the caller may write.
            let wanted: Vec<(u64, String)> = page
                .iter()
                .filter_map(|e| e.column.clone().map(|c| (e.row, c)))
                .collect();
            let cells = if wanted.is_empty() {
                Vec::new()
            } else {
                self.host
                    .cells(&self.site, &caller.user, &job, &wanted)
                    .await
            };
            let rows = page
                .into_iter()
                .map(|e| {
                    let cell = e.column.as_ref().and_then(|column| {
                        cells
                            .iter()
                            .find(|(row, c, _)| *row == e.row && c == column)
                            .map(|(_, _, text)| cut(text, MAX_CELL_TEXT))
                    });
                    ImportRowErrorOut {
                        row: e.row,
                        column: e.column,
                        cell_text: cell,
                        explanation: explain(&e.code),
                        code: e.code,
                    }
                })
                .collect();
            let next_cursor = more.then(|| format!("c{}", offset + limit));
            Ok(ImportErrorsPageOut { rows, next_cursor })
        })
    }

    fn run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        digest: &'a str,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>> {
        Box::pin(self.run_job(ctx, id, digest))
    }

    fn cancel<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>> {
        Box::pin(async move {
            let caller = self.host.caller(ctx).await?;
            let mut job = self.own(&caller, id).await?;
            match job.state {
                ImportStateOut::Cancelled => {}
                ImportStateOut::Done | ImportStateOut::Failed => {
                    self.event(&caller.user, Some(&job), "refused", "not_ready");
                    return Err(ApiError::from_code(Code::ImportNotReady));
                }
                ImportStateOut::Running => {
                    // The runner stops after the current batch and sets the state itself.
                    job.cancel_requested = true;
                    self.records.put(job.clone()).await?;
                    self.event(&caller.user, Some(&job), "cancelled", "requested");
                }
                _ => {
                    job.cancel_requested = true;
                    job.state = ImportStateOut::Cancelled;
                    self.records.put(job.clone()).await?;
                    self.host.discard(&job.id).await;
                    self.event(&caller.user, Some(&job), "cancelled", "ok");
                }
            }
            Ok(Self::out(&job))
        })
    }
}

/// `text` cut to at most `max` bytes at a character boundary.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_owned()
}

/// The user-facing sentence of a row error code (never the cell).
fn explain(code: &str) -> String {
    match code {
        "import.bad_value" => "The cell does not fit the field's type; fix it and import again.",
        "import.orphan_row" => "A child row has no parent row above it.",
        "import.key_missing" => "The row has no value for the key field.",
        "import.key_not_found" => "No document has this key, so it cannot be updated.",
        "import.duplicate_key" => "The key appears twice in the file.",
        "permission.denied" => "You may not write this document.",
        _ => "The row cannot be imported; fix it and import again.",
    }
    .to_owned()
}
