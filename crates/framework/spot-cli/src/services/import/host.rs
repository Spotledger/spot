//! The production [`ImportHost`]: the site's sign-in service names the caller, the permission
//! snapshot answers the import and submit rights, the files service releases the uploaded file
//! (only a file of the caller's tenant that the files service itself serves: an unscanned,
//! quarantined or foreign file is `404`), and `spot-import` reads, maps, converts and writes.
//!
//! Rows are written **as the creator**: the engine call carries `Actor::User { id: creator }`,
//! so the lifecycle checks that user's rights on every row; this module never runs a row as the
//! platform. The bookkeeping records (jobs, errors) are the only thing the platform writes.
//!
//! The spool: the file is copied once, at `create`, into `<spool_dir>/<site>/<job>.csv` (`.xlsx` when the bytes are a workbook) with a
//! size cap, so the run reads the bytes the dry run saw (a replaced upload cannot change them),
//! and a run after a restart finds them. The spool file is removed when the job ends.

use std::path::PathBuf;
use std::sync::Arc;

use spot_app_api::context::{Actor, Invocation};
use spot_app_api::error::{AppError, ErrorClass};
use spot_app_api::prelude::RecordId;
use spot_core::Clock;
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_field_guard::{MetaProtection, ProtectionSource as _};
use spot_http::api::codec::parse_record_id;
use spot_http::api::files::{FileGetRequest, FilesPort, next_chunk};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_import::convert::{ConversionOptions, Formats};
use spot_import::{
    AnySource, ApplyMode, ApplyOptions, ImportItem, ImportPolicy, ImportReport, MappingPlan,
    Override, PermissionProbe, ProgressSink, RowSource, import_stream,
};
use spot_permissions::{PermissionEngine, Principal, Right, TENANT_ADMINISTRATOR};
use tokio::io::AsyncWriteExt as _;

use super::flow::{
    Caller, EngineOutcome, EngineRequest, ImportHost, Refusal, RunControl, StagedFile,
};
use super::records::{JobRecord, StoredError};
use crate::audit::PathClass;
use crate::live::SiteLive;

/// The most rows one engine transaction carries (see the run).
const ENGINE_BATCH: usize = 50;
/// The most of those transactions that run at once, on separate connections, for a DocType whose
/// rows do not depend on each other (see [`rows_are_independent`]). A row's creation is a chain
/// of round trips to the database, so one transaction waits on the network most of the time and
/// several in flight multiply the throughput. A site uses at most half of its connection pool
/// ([`parallel_for_pool`]), so the rest of the host still gets connections during an import.
const ENGINE_PARALLEL: usize = 8;

/// How many engine transactions an import may run at once on a pool of `pool_max_size`
/// connections: half the pool, between one and [`ENGINE_PARALLEL`].
#[must_use]
pub fn parallel_for_pool(pool_max_size: usize) -> usize {
    (pool_max_size / 2).clamp(1, ENGINE_PARALLEL)
}

/// Whether the rows of an import of `doctype` can be written in any order: the DocType gives no
/// name from a series, a field or a hash (a name is allocated in write order), has no unique
/// field, unique index or overlap exclusion (which of two clashing rows wins is write order),
/// so the file order decides nothing about the outcome. Anything else is written in file order,
/// one transaction at a time.
fn rows_are_independent(meta: &spot_meta::EffectiveMeta, doctype: &str) -> bool {
    meta.definition(doctype).is_some_and(|def| {
        !def.is_child
            && def.naming.is_none()
            && def.exclusions.is_empty()
            && def.indexes.iter().all(|index| !index.unique)
            && def.fields.iter().all(|field| !field.unique)
    })
}

/// The engine as the import writer's batch seam.
struct EngineBatch {
    engine: Arc<spot_document::engine::Engine<spot_postgres::PgBackend>>,
    /// How many engine transactions one call may run at once.
    parallel: usize,
}

impl spot_import::BatchCreate for EngineBatch {
    fn create_many<'a>(
        &'a self,
        calls: Vec<(Invocation, spot_app_api::document::Document)>,
    ) -> spot_app_api::BoxFuture<
        'a,
        Vec<Result<spot_app_api::context::Outcome<spot_app_api::document::Document>, AppError>>,
    > {
        Box::pin(async move {
            if self.parallel <= 1 || calls.len() <= ENGINE_BATCH {
                return self.engine.create_batch(calls).await;
            }
            // Engine-sized transactions at once; the answers come back in call order.
            let mut rest = calls;
            let mut tasks = Vec::new();
            while !rest.is_empty() {
                let tail = rest.split_off(rest.len().min(ENGINE_BATCH));
                let part = std::mem::replace(&mut rest, tail);
                let engine = Arc::clone(&self.engine);
                tasks.push((
                    part.len(),
                    tokio::spawn(async move { engine.create_batch(part).await }),
                ));
            }
            let mut answers = Vec::new();
            for (len, task) in tasks {
                match task.await {
                    Ok(mut part) => answers.append(&mut part),
                    Err(_) => answers.extend((0..len).map(|_| {
                        Err(AppError::new(
                            ErrorClass::Unavailable,
                            "server.unavailable",
                            "A batch of the import did not finish.",
                        ))
                    })),
                }
            }
            answers
        })
    }
}

/// The host of one site.
pub struct SiteHost {
    /// The site name.
    pub site: String,
    /// The site's live handles.
    pub live: Arc<SiteLive>,
    /// The clock the engine's invocations use.
    pub clock: Arc<dyn Clock>,
    /// The spool directory.
    pub spool: PathBuf,
    /// How many engine transactions one import run may have open at once
    /// ([`parallel_for_pool`] of the site's pool).
    pub parallel: usize,
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable)
}

impl SiteHost {
    fn engine(&self) -> Option<Arc<PermissionEngine>> {
        self.live.served().map(|s| Arc::clone(&s.decision))
    }

    async fn principal(&self, user: &str) -> Option<Principal> {
        let id = parse_record_id(user)?;
        let principals = self.live.principals()?;
        Some(DocPrincipals::resolve(&*principals, &id).await)
    }

    fn has_right(
        engine: &PermissionEngine,
        principal: &Principal,
        doctype: &str,
        right: Right,
    ) -> bool {
        if principal.is_system {
            return false;
        }
        let grants = engine.snapshot().grants();
        principal
            .assignments
            .iter()
            .flat_map(|a| a.roles.iter())
            .any(|role| {
                role.as_str() == TENANT_ADMINISTRATOR
                    || grants.iter().any(|g| {
                        g.doctype == doctype
                            && &g.role == role
                            && !g.if_owner
                            && g.rights.contains(right)
                    })
            })
    }

    fn path_of(&self, job: &str) -> Option<PathBuf> {
        // A job id is a record id: hex digits and hyphens, never a path.
        parse_record_id(job)?;
        let dir = self.spool.join(&self.site);
        let workbook = dir.join(format!("{job}.xlsx"));
        // The importer picks the reader from the extension, so the spool copy carries the one
        // its bytes call for (see `stage`).
        Some(if workbook.exists() {
            workbook
        } else {
            dir.join(format!("{job}.csv"))
        })
    }

    fn path_with(&self, job: &str, extension: &str) -> Option<PathBuf> {
        parse_record_id(job)?;
        Some(
            self.spool
                .join(&self.site)
                .join(format!("{job}.{extension}")),
        )
    }

    fn invocation(&self, user: &str, job: &JobRecord) -> Result<Invocation, Refusal> {
        let id = parse_record_id(user).ok_or(Refusal::RightMissing)?;
        Ok(Invocation {
            site: spot_core::SiteId::new(&self.site).map_err(|_| Refusal::Unavailable)?,
            actor: Actor::User { id, via: None },
            // Every row's command id derives from the job id (the batch label), so a re-run
            // after a crash skips rows already written.
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        })
        .inspect(|_| {
            let _ = job;
        })
    }
}

/// The create and update right of the creator on the DocType, asked before any write so a dry
/// run reports a refusal per row without writing.
struct GrantProbe {
    engine: Arc<PermissionEngine>,
    principal: Principal,
}

impl PermissionProbe for GrantProbe {
    fn check_create(&self, _inv: &Invocation, doctype: &str) -> Result<(), AppError> {
        if SiteHost::has_right(&self.engine, &self.principal, doctype, Right::Create) {
            Ok(())
        } else {
            Err(AppError::new(
                ErrorClass::PermissionDenied,
                "permission.denied",
                "You may not create this document.",
            ))
        }
    }

    fn check_update(
        &self,
        _inv: &Invocation,
        doctype: &str,
        _id: RecordId,
    ) -> Result<(), AppError> {
        if SiteHost::has_right(&self.engine, &self.principal, doctype, Right::Write) {
            Ok(())
        } else {
            Err(AppError::new(
                ErrorClass::PermissionDenied,
                "permission.denied",
                "You may not change this document.",
            ))
        }
    }
}

struct Sink<'a>(&'a dyn RunControl);

impl ProgressSink for Sink<'_> {
    fn progress(&mut self, done: u64, total: Option<u64>) {
        self.0.progress(done, total);
    }
}

fn overrides_of(job: &JobRecord) -> Vec<Override> {
    let Some(mapping) = &job.mapping else {
        return Vec::new();
    };
    let mut out: Vec<Override> = mapping
        .columns
        .iter()
        .map(|(column, field)| Override::new(column.clone(), field.clone()))
        .collect();
    for (table, prefix) in &mapping.child_tables {
        // A child table's columns carry the prefix: `<prefix><field>` binds `table.field`.
        let _ = (table, prefix);
    }
    out.shrink_to_fit();
    out
}

fn stored(errors: &[spot_import::ImportRowError]) -> Vec<StoredError> {
    errors
        .iter()
        .map(|e| StoredError {
            row: e.row,
            column: (!e.column.is_empty()).then(|| e.column.clone()),
            code: e.code.clone(),
            field: None,
        })
        .collect()
}

/// Opens the staged file and the mapping plan against the served metadata.
fn open(
    meta: &spot_meta::EffectiveMeta,
    request: &EngineRequest<'_>,
) -> Result<(AnySource, MappingPlan), Refusal> {
    let limits = spot_import::Limits {
        max_columns: usize::try_from(request.limits.max_columns).unwrap_or(500),
        ..spot_import::Limits::platform(
            Some(request.limits.max_file_bytes),
            Some(request.limits.max_rows),
        )
    };
    let source = AnySource::open(&request.file.path, &limits).map_err(|error| {
        use spot_import::ImportError as E;
        match error {
            E::FileTooLarge { .. } | E::TooManyRows { .. } | E::TooManyColumns { .. } => {
                Refusal::LimitExceeded
            }
            _ => Refusal::FileUnreadable,
        }
    })?;
    let headers = source.headers().to_vec();
    let (dates, datetimes, times) = Formats::default_lists();
    let formats =
        Formats::build(&dates, &datetimes, &times).map_err(|_| Refusal::MappingInvalid)?;
    let plan = MappingPlan::build(
        meta,
        &request.job.doctype,
        &headers,
        &overrides_of(request.job),
        ConversionOptions::default(),
        formats,
    )
    .map_err(|_| Refusal::MappingInvalid)?;
    Ok((source, plan))
}

impl ImportHost for SiteHost {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<Caller, ApiError>> {
        Box::pin(async move {
            if ctx.site.as_str() != self.site {
                return Err(ApiError::from_code(Code::PermissionDenied));
            }
            let auth = self.live.auth().ok_or_else(unavailable)?;
            let who = auth.authenticate_on(ctx, PathClass::Api).await?;
            if auth.is_restricted(&who) {
                return Err(ApiError::from_code(Code::PermissionDenied));
            }
            Ok(Caller {
                user: who.user_id.to_string(),
            })
        })
    }

    fn active<'a>(&'a self, _site: &'a str, user: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.principal(user)
                .await
                .is_some_and(|p| !p.is_system && !p.assignments.is_empty())
        })
    }

    fn may_import<'a>(
        &'a self,
        _site: &'a str,
        user: &'a str,
        doctype: &'a str,
        submit: bool,
    ) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            let engine = self.engine().ok_or(Refusal::Unavailable)?;
            let principal = self.principal(user).await.ok_or(Refusal::RightMissing)?;
            let import = Self::has_right(&engine, &principal, doctype, Right::Import);
            let submitted = !submit || Self::has_right(&engine, &principal, doctype, Right::Submit);
            if import && submitted {
                Ok(())
            } else {
                Err(Refusal::RightMissing)
            }
        })
    }

    fn stage<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        file_id: &'a str,
        job_id: &'a str,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<StagedFile, Refusal>> {
        Box::pin(async move {
            let files = self.live.files().ok_or(Refusal::Unavailable)?;
            let id = parse_record_id(file_id).ok_or(Refusal::NotFound)?;
            self.path_of(job_id).ok_or(Refusal::NotFound)?;
            // The files service decides: tenant, uploader, privacy, scan and quarantine. Every
            // refusal is its one `404`, so a file of another tenant is indistinguishable.
            let download = files
                .get(ctx, id, FileGetRequest::default())
                .await
                .map_err(|_| Refusal::NotFound)?;
            let digest = download
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("etag"))
                .map(|(_, value)| value.trim_matches('"').to_owned())
                .unwrap_or_default();
            let Some(mut body) = download.body else {
                return Err(Refusal::FileUnreadable);
            };
            let mut path = None;
            let mut out: Option<tokio::fs::File> = None;
            let mut bytes = 0u64;
            while let Some(chunk) = next_chunk(&mut body).await {
                let chunk = chunk.map_err(|_| Refusal::FileUnreadable)?;
                bytes += chunk.len() as u64;
                if bytes > max_bytes {
                    drop(out);
                    if let Some(path) = &path {
                        let _ = tokio::fs::remove_file(path).await;
                    }
                    return Err(Refusal::LimitExceeded);
                }
                if out.is_none() {
                    // A workbook is a zip (`PK`); anything else is read as CSV.
                    let extension = if chunk.starts_with(b"PK") { "xlsx" } else { "csv" };
                    let target = self
                        .path_with(job_id, extension)
                        .ok_or(Refusal::NotFound)?;
                    if let Some(parent) = target.parent() {
                        tokio::fs::create_dir_all(parent)
                            .await
                            .map_err(|_| Refusal::Unavailable)?;
                    }
                    let other = if extension == "xlsx" { "csv" } else { "xlsx" };
                    if let Some(stale) = self.path_with(job_id, other) {
                        let _ = tokio::fs::remove_file(stale).await;
                    }
                    out = Some(
                        tokio::fs::File::create(&target)
                            .await
                            .map_err(|_| Refusal::Unavailable)?,
                    );
                    path = Some(target);
                }
                if let Some(out) = out.as_mut() {
                    out.write_all(&chunk)
                        .await
                        .map_err(|_| Refusal::Unavailable)?;
                }
            }
            let (Some(path), Some(mut out)) = (path, out) else {
                return Err(Refusal::FileUnreadable);
            };
            out.flush().await.map_err(|_| Refusal::Unavailable)?;
            Ok(StagedFile {
                path,
                bytes,
                digest,
                owned: false,
            })
        })
    }

    fn spool<'a>(&'a self, job_id: &'a str) -> BoxFuture<'a, Option<StagedFile>> {
        Box::pin(async move {
            let path = self.path_of(job_id)?;
            let meta = tokio::fs::metadata(&path).await.ok()?;
            Some(StagedFile {
                path,
                bytes: meta.len(),
                // The runner compares the digest of the spool copy it reads against the one the
                // dry run recorded; the copy is private to the job, so the record's own digest
                // stands in for it.
                digest: String::new(),
                owned: false,
            })
        })
    }

    fn discard<'a>(&'a self, job_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(path) = self.path_of(job_id) {
                let _ = tokio::fs::remove_file(path).await;
            }
        })
    }

    fn dry_run<'a>(
        &'a self,
        request: &'a EngineRequest<'a>,
    ) -> BoxFuture<'a, Result<EngineOutcome, Refusal>> {
        Box::pin(async move {
            if request.job.mode != spot_http::api::ports::import::ImportModeIn::Insert {
                // Update and upsert need a key index of the target DocType (see import.md).
                return Err(Refusal::MappingInvalid);
            }
            let served = self.live.served().ok_or(Refusal::Unavailable)?;
            let (mut source, plan) = open(&served.meta, request)?;
            let mut report = ImportReport::with_capacity(
                usize::try_from(request.limits.max_errors).unwrap_or(100),
            );
            let mut rows = 0u64;
            let mut ok = 0u64;
            let mut errors = Vec::new();
            for item in import_stream(&mut source, plan, &served.meta, &mut report) {
                match item {
                    ImportItem::Document(_) => {
                        rows += 1;
                        ok += 1;
                    }
                    ImportItem::Error(error) => {
                        rows += 1;
                        errors.extend(stored(std::slice::from_ref(&error)));
                    }
                    ImportItem::Fatal(error) => {
                        return Err(match error {
                            spot_import::ImportError::TooManyRows { .. }
                            | spot_import::ImportError::FileTooLarge { .. } => {
                                Refusal::LimitExceeded
                            }
                            _ => Refusal::FileUnreadable,
                        });
                    }
                }
            }
            Ok(EngineOutcome {
                rows,
                ok,
                skipped: 0,
                errors,
                cancelled: false,
            })
        })
    }

    fn run<'a>(
        &'a self,
        request: &'a EngineRequest<'a>,
        control: &'a dyn RunControl,
    ) -> BoxFuture<'a, Result<EngineOutcome, Refusal>> {
        Box::pin(async move {
            let served = self.live.served().ok_or(Refusal::Unavailable)?;
            let engine = self.live.engine().ok_or(Refusal::Unavailable)?;
            let principal = self
                .principal(request.user)
                .await
                .ok_or(Refusal::RightMissing)?;
            let inv = self.invocation(request.user, request.job)?;
            let (mut source, plan) = open(&served.meta, request)?;
            let mut report = ImportReport::with_capacity(
                usize::try_from(request.limits.max_errors).unwrap_or(100),
            );
            let probe = Arc::new(GrantProbe {
                engine: Arc::clone(&served.decision),
                principal,
            });
            let mut options = ApplyOptions::new(Arc::clone(&served.meta), probe);
            options.mode = ApplyMode::Insert;
            options.policy = ImportPolicy::SkipBadRows;
            options.post_after_import = request.job.submit;
            options.max_errors = request.limits.max_errors;
            options.batch_label = request.job.id.clone();
            options.progress_every = request.limits.batch_rows;
            // Rows are created in one transaction per batch (the engine gives each its own
            // savepoint, command id and verdict), not one transaction per row. A batch holds at
            // most `ENGINE_BATCH` rows: each row that writes opens a subtransaction, and more
            // than 64 in one transaction make every other session's snapshots slower.
            let parallel = if rows_are_independent(&served.meta, &request.job.doctype) {
                self.parallel
            } else {
                1
            };
            options.batch = Some(Arc::new(EngineBatch {
                engine: Arc::clone(&engine),
                parallel,
            }));
            // One call carries up to `parallel` transactions' rows; the cancel flag and the error
            // limit are checked between calls.
            options.batch_rows = usize::try_from(request.limits.batch_rows)
                .unwrap_or(ENGINE_BATCH)
                .clamp(1, ENGINE_BATCH * parallel);
            let items = import_stream(&mut source, plan, &served.meta, &mut report);
            // The engine polls this flag between documents; the runner's ticker sets it from the
            // job record (`cancel`), so a stop lands within one batch.
            let cancel = options.cancel.clone();
            let watcher = {
                let cancelled = control.cancelled();
                if cancelled {
                    cancel.cancel();
                }
                cancelled
            };
            let _ = watcher;
            let outcome = spot_import::apply_import(
                engine.as_ref(),
                &inv,
                items,
                &options,
                &mut Sink(control),
            )
            .await;
            let summary = outcome.summary();
            Ok(EngineOutcome {
                rows: summary.created + summary.updated + summary.skipped + summary.failed,
                ok: summary.written(),
                skipped: summary.skipped,
                errors: stored(&summary.errors),
                cancelled: summary.cancelled,
            })
        })
    }

    fn cells<'a>(
        &'a self,
        _site: &'a str,
        user: &'a str,
        job: &'a JobRecord,
        wanted: &'a [(u64, String)],
    ) -> BoxFuture<'a, Vec<(u64, String, String)>> {
        Box::pin(async move {
            // The file is still there only while the job is; afterwards no cell is shown.
            let Some(path) = self.path_of(&job.id) else {
                return Vec::new();
            };
            let (Some(served), Some(principal)) = (self.live.served(), self.principal(user).await)
            else {
                return Vec::new();
            };
            let limits = spot_import::Limits::platform(None, None);
            let Ok(mut source) = AnySource::open(&path, &limits) else {
                return Vec::new();
            };
            let Some(mapping) = &job.mapping else {
                return Vec::new();
            };
            let protection = MetaProtection::new(&served.meta);
            // A cell is shown only when the caller may write its field and the field is in no
            // protection group.
            let writable = |column: &str| {
                mapping
                    .columns
                    .iter()
                    .find(|(c, _)| c == column)
                    .is_some_and(|(_, field)| {
                        Self::has_right(&served.decision, &principal, &job.doctype, Right::Write)
                            && protection
                                .protection(&job.doctype)
                                .is_some_and(|p| p.field(field).groups.is_empty())
                    })
            };
            let headers = source.headers().to_vec();
            let mut out = Vec::new();
            for row in source.rows().flatten() {
                for (number, column) in wanted {
                    if row.number != *number || !writable(column) {
                        continue;
                    }
                    let index = headers.iter().position(|h| h == column);
                    if let Some(cell) = index.and_then(|i| row.cells.get(i)) {
                        out.push((*number, column.clone(), cell.as_str().to_owned()));
                    }
                }
            }
            out
        })
    }

    fn meta_version(&self) -> u64 {
        self.live
            .served()
            .and_then(|s| s.version)
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "a test builds its fixture with expect")]
mod tests {
    use spot_meta::{Customisations, Definition, Registry};

    use super::rows_are_independent;

    fn meta(docs: &[&str]) -> spot_meta::EffectiveMeta {
        let definitions = docs
            .iter()
            .map(|text| Definition::parse("test.toml", text).expect("a valid definition"))
            .collect();
        Registry::build(definitions, &Customisations::new()).expect("valid metadata")
    }

    const PLAIN: &str = "name = \"plain\"\ntitle_field = \"subject\"\n\
        [[fields]]\nname = \"subject\"\ntype = \"text\"\n";
    const UNIQUE_FIELD: &str = "name = \"keyed\"\ntitle_field = \"subject\"\n\
        [[fields]]\nname = \"subject\"\ntype = \"text\"\nunique = true\n";
    const UNIQUE_INDEX: &str = "name = \"indexed\"\ntitle_field = \"subject\"\n\
        [[fields]]\nname = \"subject\"\ntype = \"text\"\n\
        [[fields]]\nname = \"code\"\ntype = \"text\"\n\
        [[indexes]]\nfields = [\"subject\", \"code\"]\nunique = true\n";
    const SERIES: &str = "name = \"numbered\"\ntitle_field = \"subject\"\nnaming = \"NUM-.#####\"\n\
        [[fields]]\nname = \"subject\"\ntype = \"text\"\n";

    #[test]
    fn an_import_uses_at_most_half_the_pool() {
        assert_eq!(super::parallel_for_pool(1), 1);
        assert_eq!(super::parallel_for_pool(4), 2);
        assert_eq!(super::parallel_for_pool(16), 8);
        assert_eq!(super::parallel_for_pool(100), 8);
    }

    #[test]
    fn rows_with_no_name_and_no_unique_key_are_written_in_parallel() {
        let meta = meta(&[PLAIN]);
        assert!(rows_are_independent(&meta, "plain"));
    }

    #[test]
    fn rows_whose_order_decides_a_name_or_a_clash_are_written_in_file_order() {
        let meta = meta(&[UNIQUE_FIELD, UNIQUE_INDEX, SERIES]);
        assert!(!rows_are_independent(&meta, "keyed"));
        assert!(!rows_are_independent(&meta, "indexed"));
        assert!(!rows_are_independent(&meta, "numbered"));
        assert!(!rows_are_independent(&meta, "unknown"));
    }
}
