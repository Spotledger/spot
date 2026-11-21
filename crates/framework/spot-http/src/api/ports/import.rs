//! The data import seam of `/api/v1/import*` (`import_limits`, `import_create`, `import_get`,
//! `import_errors`, `import_run`, `import_cancel`, contract 0.21.0, WP-CON-03.T50): a two phase
//! import of an uploaded file, a dry run that produces a digest and a run that needs it.
//!
//! `spot-http` does not depend on the import engine. It defines [`ImportPort`], the operations
//! in the shapes the router needs, and the host writes the adapter over `spot-import` (the
//! import service of WP-DOC-09.T5). Everything that decides is the port's: who the caller is
//! (from `ctx.credential` on `ctx.site`), the import and submit rights, whether a file belongs
//! to the caller's tenant and is clean, which cells the caller may see, and whether a job is the
//! caller's. The routes pass **no user and no tenant**: there is no way to ask for another
//! caller's job through this trait, and a job that is not the caller's is
//! `404 document.not_found`, the answer for a job that does not exist.
//!
//! **Idempotency.** The registered route groups sit outside the router's `Idempotency-Key`
//! middleware, so the route checks the key's shape and hands it to the port in
//! `ctx.idempotency_key`; the port answers a repeat of `create` or `run` with the same key and
//! the same request with the first answer, and with `api.idempotency_conflict` when the request
//! differs. [`InMemoryImportPort`] does it, and the host adapter must.
//!
//! [`InMemoryImportPort`] is the deterministic fake the tests use (an injected clock, ids from a
//! counter, a digest over the request).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use sha2::{Digest as _, Sha256};
use spot_core::Clock;
use time::format_description::well_known::Rfc3339;

use super::PortKey;
use crate::api::registry::Code;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`ImportPort`].
pub const KEY: PortKey = PortKey::of::<dyn ImportPort>();

/// What a port call answers.
pub type ImportResult<T> = Result<T, ApiError>;

/// The most `errors` rows in a page (the contract's `maximum`).
pub const MAX_ERRORS_PAGE: u32 = 200;
/// The default `errors` page size.
pub const DEFAULT_ERRORS_PAGE: u32 = 50;
/// The longest `cell_text` the contract allows, in bytes.
pub const MAX_CELL_TEXT: usize = 256;
/// The most column entries in a mapping (the contract's `maxItems`).
pub const MAX_MAPPING_COLUMNS: usize = 500;
/// The most child table entries in a mapping (the contract's `maxProperties`).
pub const MAX_CHILD_TABLES: usize = 20;

/// The import mode (the contract's `mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportModeIn {
    /// Only new documents.
    Insert,
    /// Only existing documents, by key.
    Update,
    /// Update when the key exists, insert otherwise.
    Upsert,
}

impl ImportModeIn {
    /// The contract's text.
    pub fn as_str(self) -> &'static str {
        match self {
            ImportModeIn::Insert => "insert",
            ImportModeIn::Update => "update",
            ImportModeIn::Upsert => "upsert",
        }
    }

    /// Parses the contract's text.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "insert" => Some(ImportModeIn::Insert),
            "update" => Some(ImportModeIn::Update),
            "upsert" => Some(ImportModeIn::Upsert),
            _ => None,
        }
    }
}

/// The column mapping (the contract's `ImportMapping`), shape-checked by the route.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportMappingIn {
    /// `(column, field)` pairs, in the order sent.
    pub columns: Vec<(String, String)>,
    /// Child table field to its column prefix.
    pub child_tables: BTreeMap<String, String>,
}

/// The body of `import_create` (the contract's `ImportCreateRequest`), shape-checked by the route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportCreateIn {
    /// The id of a file already uploaded through `file_upload`.
    pub file_id: String,
    /// The DocType's storage name.
    pub doctype: String,
    /// The mode.
    pub mode: ImportModeIn,
    /// The mapping; `None` lets the port map columns by header.
    pub mapping: Option<ImportMappingIn>,
    /// Whether the rows are submitted as well as saved.
    pub submit: bool,
    /// `true` (the default) is a dry run; `false` asks to run without one, which the port
    /// refuses as `import.not_ready`: a run needs a dry run's digest.
    pub dry_run: bool,
}

/// The job states (the contract's `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportStateOut {
    /// Accepted, not started.
    Queued,
    /// The dry run is reading.
    DryRunning,
    /// The dry run finished; a run may start with its digest.
    Ready,
    /// The run is writing.
    Running,
    /// The run finished.
    Done,
    /// The job stopped on an error of its own.
    Failed,
    /// The job was cancelled.
    Cancelled,
}

impl ImportStateOut {
    /// The contract's text.
    pub fn as_str(self) -> &'static str {
        match self {
            ImportStateOut::Queued => "queued",
            ImportStateOut::DryRunning => "dry_running",
            ImportStateOut::Ready => "ready",
            ImportStateOut::Running => "running",
            ImportStateOut::Done => "done",
            ImportStateOut::Failed => "failed",
            ImportStateOut::Cancelled => "cancelled",
        }
    }
}

/// The row counts of a job.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportCountsOut {
    /// Rows read.
    pub rows: u64,
    /// Rows written (or, in a dry run, that would be).
    pub ok: u64,
    /// Rows with an error.
    pub errors: u64,
    /// Rows left alone.
    pub skipped: u64,
}

/// One job (the contract's `ImportJob`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportJobOut {
    /// The job id (a record id).
    pub id: String,
    /// The DocType's storage name.
    pub doctype: String,
    /// The mode.
    pub mode: ImportModeIn,
    /// The state.
    pub state: ImportStateOut,
    /// The mapping the job uses.
    pub mapping: Option<ImportMappingIn>,
    /// Whether rows are submitted.
    pub submit: Option<bool>,
    /// The counts.
    pub counts: ImportCountsOut,
    /// Progress per mille, 0 to 1000.
    pub progress: u32,
    /// The dry run's digest (64 hex characters); `None` before the dry run finished.
    pub digest: Option<String>,
    /// The creator's id (a record id).
    pub created_by: String,
    /// When the job was created: a UTC RFC 3339 instant.
    pub created_at: String,
}

/// One row error (an item of the contract's `ImportErrorsResponse`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRowErrorOut {
    /// The row number in the file, from 1.
    pub row: u64,
    /// The column, when the error is about one.
    pub column: Option<String>,
    /// The cell as the user wrote it. The route cuts it to [`MAX_CELL_TEXT`] bytes; the port
    /// leaves it `None` when the caller could not write the field.
    pub cell_text: Option<String>,
    /// The error's dotted code.
    pub code: String,
    /// The user-facing explanation, at most 500 characters (the route cuts it).
    pub explanation: String,
}

/// One page of row errors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportErrorsPageOut {
    /// The rows, at most the requested limit.
    pub rows: Vec<ImportRowErrorOut>,
    /// The opaque cursor of the next page; `None` on the last.
    pub next_cursor: Option<String>,
}

/// The limits of the site (the contract's `ImportLimitsResponse`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportLimitsOut {
    /// The most rows in a file.
    pub max_rows: u64,
    /// The largest file in bytes.
    pub max_bytes: u64,
    /// The most columns in a row.
    pub max_columns: u64,
    /// The modes the site allows.
    pub modes: Vec<ImportModeIn>,
}

/// Data import for the caller the credential names.
pub trait ImportPort: Send + Sync {
    /// The limits of this site.
    fn limits<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, ImportResult<ImportLimitsOut>>;

    /// Starts a job and runs its dry run.
    fn create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: ImportCreateIn,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>>;

    /// One job; `document.not_found` for a job that is not the caller's.
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>>;

    /// One page of a job's row errors, `limit` between 1 and [`MAX_ERRORS_PAGE`].
    fn errors<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, ImportResult<ImportErrorsPageOut>>;

    /// Runs a job in state `ready` whose dry run digest is `digest`.
    fn run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        digest: &'a str,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>>;

    /// Cancels a job; it stops after the current batch.
    fn cancel<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>>;
}

// ---------------------------------------------------------------------------------------------
// The in-memory port

/// One scripted file of [`InMemoryImportPort`].
#[derive(Clone, Debug)]
pub struct MemoryFile {
    /// The rows the file holds.
    pub rows: u64,
    /// The columns of each row.
    pub columns: u64,
    /// The size in bytes.
    pub bytes: u64,
    /// The row errors a dry run finds, in row order.
    pub errors: Vec<MemoryRowError>,
}

/// One scripted row error.
#[derive(Clone, Debug)]
pub struct MemoryRowError {
    /// The row, from 1.
    pub row: u64,
    /// The column.
    pub column: Option<String>,
    /// The cell text; shown only when `writable`.
    pub cell: String,
    /// Whether the caller could write the field the cell is for.
    pub writable: bool,
    /// The code.
    pub code: String,
}

struct Job {
    site: String,
    owner: String,
    out: ImportJobOut,
    errors: Vec<MemoryRowError>,
}

struct Replay {
    fingerprint: String,
    job: ImportJobOut,
}

struct Limits {
    max_rows: u64,
    max_bytes: u64,
    max_columns: u64,
}

#[derive(Default)]
struct State {
    /// (site, token) to the user's record id.
    tokens: HashMap<(String, String), String>,
    /// (site, user, doctype) holding the import right.
    rights: Vec<(String, String, String)>,
    /// (site, user) holding the submit right.
    submitters: Vec<(String, String)>,
    /// (site, file id) to the file and the user it belongs to.
    files: HashMap<(String, String), (String, MemoryFile)>,
    jobs: Vec<Job>,
    /// (site, user, operation, key) to the first answer.
    replays: HashMap<(String, String, &'static str, String), Replay>,
    next: u64,
}

/// A deterministic in-memory [`ImportPort`] for tests. It decides what the real adapter
/// decides, in the simplest way: a token names a user, a user holds the import right per
/// DocType, a file belongs to one user of one site, and a job is its creator's alone.
pub struct InMemoryImportPort {
    clock: Arc<dyn Clock>,
    limits: Limits,
    state: Mutex<State>,
}

impl InMemoryImportPort {
    /// A port with the default limits (100,000 rows, 50 MiB, 200 columns).
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        InMemoryImportPort {
            clock,
            limits: Limits {
                max_rows: 100_000,
                max_bytes: 50 * 1024 * 1024,
                max_columns: 200,
            },
            state: Mutex::new(State::default()),
        }
    }

    /// Replaces the limits.
    pub fn with_limits(mut self, max_rows: u64, max_bytes: u64, max_columns: u64) -> Self {
        self.limits = Limits {
            max_rows,
            max_bytes,
            max_columns,
        };
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Lets `token` act as `user` (a record id) on `site`.
    pub fn grant_token(&self, site: &str, token: &str, user: &str) {
        self.lock()
            .tokens
            .insert((site.to_owned(), token.to_owned()), user.to_owned());
    }

    /// Gives `user` the import right on `doctype`.
    pub fn grant_import(&self, site: &str, user: &str, doctype: &str) {
        self.lock()
            .rights
            .push((site.to_owned(), user.to_owned(), doctype.to_owned()));
    }

    /// Gives `user` the submit right.
    pub fn grant_submit(&self, site: &str, user: &str) {
        self.lock()
            .submitters
            .push((site.to_owned(), user.to_owned()));
    }

    /// Registers an uploaded, clean file of `owner` on `site`.
    pub fn add_file(&self, site: &str, owner: &str, file_id: &str, file: MemoryFile) {
        self.lock().files.insert(
            (site.to_owned(), file_id.to_owned()),
            (owner.to_owned(), file),
        );
    }

    fn not_found() -> ApiError {
        ApiError::from_code(Code::DocumentNotFound)
    }

    fn user_of(state: &State, ctx: &RequestCtx) -> ImportResult<(String, String)> {
        let site = ctx.site.as_str().to_owned();
        let token = ctx
            .credential
            .as_ref()
            .map(|c| c.token.clone())
            .ok_or_else(|| ApiError::from_code(Code::AuthRequired))?;
        let user = state
            .tokens
            .get(&(site.clone(), token))
            .cloned()
            .ok_or_else(|| ApiError::from_code(Code::AuthRequired))?;
        Ok((site, user))
    }

    fn stamp(&self) -> String {
        self.clock
            .now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
    }

    fn digest_of(request: &ImportCreateIn) -> String {
        let mut hasher = Sha256::new();
        hasher.update(request.file_id.as_bytes());
        hasher.update([0]);
        hasher.update(request.doctype.as_bytes());
        hasher.update([0]);
        hasher.update(request.mode.as_str().as_bytes());
        hasher.update([u8::from(request.submit)]);
        if let Some(mapping) = &request.mapping {
            for (column, field) in &mapping.columns {
                hasher.update(column.as_bytes());
                hasher.update([1]);
                hasher.update(field.as_bytes());
                hasher.update([2]);
            }
            for (table, prefix) in &mapping.child_tables {
                hasher.update(table.as_bytes());
                hasher.update([3]);
                hasher.update(prefix.as_bytes());
                hasher.update([4]);
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

    fn job_mut<'s>(
        state: &'s mut State,
        site: &str,
        user: &str,
        id: &str,
    ) -> ImportResult<&'s mut Job> {
        state
            .jobs
            .iter_mut()
            .find(|j| j.site == site && j.owner == user && j.out.id == id)
            .ok_or_else(Self::not_found)
    }
}

impl ImportPort for InMemoryImportPort {
    fn limits<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, ImportResult<ImportLimitsOut>> {
        Box::pin(async move {
            Self::user_of(&self.lock(), ctx)?;
            Ok(ImportLimitsOut {
                max_rows: self.limits.max_rows,
                max_bytes: self.limits.max_bytes,
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
        Box::pin(async move {
            let mut state = self.lock();
            let (site, user) = Self::user_of(&state, ctx)?;
            let fingerprint = Self::fingerprint(&request);
            if let Some(key) = &ctx.idempotency_key
                && let Some(seen) =
                    state
                        .replays
                        .get(&(site.clone(), user.clone(), "create", key.clone()))
            {
                return if seen.fingerprint == fingerprint {
                    Ok(seen.job.clone())
                } else {
                    Err(ApiError::from_code(Code::ApiIdempotencyConflict))
                };
            }
            if !state
                .rights
                .iter()
                .any(|(s, u, d)| *s == site && *u == user && *d == request.doctype)
                || (request.submit
                    && !state
                        .submitters
                        .iter()
                        .any(|(s, u)| *s == site && *u == user))
            {
                return Err(ApiError::from_code(Code::ImportRightMissing));
            }
            // A file of another site or another user is the answer for a file that is not there.
            let file = match state.files.get(&(site.clone(), request.file_id.clone())) {
                Some((owner, file)) if *owner == user => file.clone(),
                _ => return Err(Self::not_found()),
            };
            if file.rows > self.limits.max_rows
                || file.bytes > self.limits.max_bytes
                || file.columns > self.limits.max_columns
            {
                return Err(ApiError::from_code(Code::ImportLimitExceeded));
            }
            if !request.dry_run {
                return Err(ApiError::new(
                    Code::ImportNotReady,
                    "A run needs the digest of a dry run.",
                ));
            }
            state.next += 1;
            let id = format!("0192f3c4-0000-7000-8000-{:012x}", state.next);
            let errors = file.errors.len() as u64;
            let out = ImportJobOut {
                id,
                doctype: request.doctype.clone(),
                mode: request.mode,
                state: ImportStateOut::Ready,
                mapping: request.mapping.clone(),
                submit: Some(request.submit),
                counts: ImportCountsOut {
                    rows: file.rows,
                    ok: file.rows.saturating_sub(errors),
                    errors,
                    skipped: 0,
                },
                progress: 0,
                digest: Some(Self::digest_of(&request)),
                created_by: user.clone(),
                created_at: self.stamp(),
            };
            state.jobs.push(Job {
                site: site.clone(),
                owner: user.clone(),
                out: out.clone(),
                errors: file.errors,
            });
            if let Some(key) = &ctx.idempotency_key {
                state.replays.insert(
                    (site, user, "create", key.clone()),
                    Replay {
                        fingerprint,
                        job: out.clone(),
                    },
                );
            }
            Ok(out)
        })
    }

    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>> {
        Box::pin(async move {
            let mut state = self.lock();
            let (site, user) = Self::user_of(&state, ctx)?;
            Ok(Self::job_mut(&mut state, &site, &user, id)?.out.clone())
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
            let mut state = self.lock();
            let (site, user) = Self::user_of(&state, ctx)?;
            let job = Self::job_mut(&mut state, &site, &user, id)?;
            let start = match cursor {
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
            let end = start.saturating_add(limit).min(job.errors.len());
            let rows = job
                .errors
                .get(start..end)
                .unwrap_or_default()
                .iter()
                .map(|e| ImportRowErrorOut {
                    row: e.row,
                    column: e.column.clone(),
                    cell_text: e.writable.then(|| e.cell.clone()),
                    code: e.code.clone(),
                    explanation: "The cell cannot be imported; fix it and import again.".to_owned(),
                })
                .collect();
            let next_cursor = (end < job.errors.len()).then(|| format!("c{end}"));
            Ok(ImportErrorsPageOut { rows, next_cursor })
        })
    }

    fn run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        digest: &'a str,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>> {
        Box::pin(async move {
            let mut state = self.lock();
            let (site, user) = Self::user_of(&state, ctx)?;
            let fingerprint = format!("{id}:{digest}");
            if let Some(key) = &ctx.idempotency_key
                && let Some(seen) =
                    state
                        .replays
                        .get(&(site.clone(), user.clone(), "run", key.clone()))
            {
                return if seen.fingerprint == fingerprint {
                    Ok(seen.job.clone())
                } else {
                    Err(ApiError::from_code(Code::ApiIdempotencyConflict))
                };
            }
            let job = Self::job_mut(&mut state, &site, &user, id)?;
            match job.out.state {
                ImportStateOut::Ready => {}
                ImportStateOut::Cancelled => {
                    return Err(ApiError::from_code(Code::ImportCancelled));
                }
                _ => return Err(ApiError::from_code(Code::ImportNotReady)),
            }
            if job.out.digest.as_deref() != Some(digest) {
                return Err(ApiError::from_code(Code::ImportDigestChanged));
            }
            job.out.state = ImportStateOut::Running;
            job.out.progress = 0;
            let out = job.out.clone();
            if let Some(key) = &ctx.idempotency_key {
                state.replays.insert(
                    (site, user, "run", key.clone()),
                    Replay {
                        fingerprint,
                        job: out.clone(),
                    },
                );
            }
            Ok(out)
        })
    }

    fn cancel<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, ImportResult<ImportJobOut>> {
        Box::pin(async move {
            let mut state = self.lock();
            let (site, user) = Self::user_of(&state, ctx)?;
            let job = Self::job_mut(&mut state, &site, &user, id)?;
            match job.out.state {
                // A repeat is a repeat of the action.
                ImportStateOut::Cancelled => {}
                ImportStateOut::Done | ImportStateOut::Failed => {
                    return Err(ApiError::from_code(Code::ImportNotReady));
                }
                _ => job.out.state = ImportStateOut::Cancelled,
            }
            Ok(job.out.clone())
        })
    }
}
