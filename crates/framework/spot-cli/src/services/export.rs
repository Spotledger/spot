//! Bulk export and incremental extraction of one site: `export_start`, `export_next` and
//! `export_extract` (`/api/v1/export*`, contract 0.4.0, WP-INT-03), over `spot-bulk-data`.
//!
//! `spot-http` owns the routes ([`BulkPort`]); `spot-bulk-data` owns the rules (the plan, keyset
//! paging, signed cursors, the guarded columns, tombstones). This module supplies the rest:
//!
//! - **who**: the site's sign-in service resolves the bearer credential; a session that has not
//!   chosen its password yet may do nothing here;
//! - **the `export` right**: a DocType right of the role grants (`Right::Export`), checked on
//!   the permission snapshot served *now* on every request, cursors included. `Tenant
//!   Administrator` holds it by name; an `if_owner` grant does not count. Without it: `403`,
//!   never an empty success;
//! - **what leaves**: rows the caller may read (the read filter is in every page query),
//!   columns the Export channel of the field guard shows, masked ones masked. A column that is
//!   unknown or refused is an error naming it, never dropped; so is a filter or sort on a field
//!   the caller may not use. A `Secret` field never leaves;
//! - **tokens**: cursors and watermarks are signed with per-site keys made when the service
//!   starts and never stored. They bind the caller and the site; a cursor carries the request it
//!   continues. A restart of the process ends them all (`422 bulk.cursor_invalid`);
//! - **bounds**: rows and bytes per chunk, exports in flight per site and per caller, and a
//!   rows-per-second budget (`429 bulk.limit` with `Retry-After`). A slot is held for one
//!   request: an export holds nothing between chunks.
//!
//! # Level
//!
//! Optional. A failed start-up check degrades the site and the routes answer `503`. The
//! settings are `services.export` (the module's name).
//!
//! # Audit
//!
//! Target `spot::audit::bulk_export`: a line for every start (a refused one with its reason
//! code) and one when an export finishes or fails: who, DocType, counts and a digest that names
//! the filter without its values. Never a value, a cursor, a watermark or a key. The honest
//! limits are in `docs/running-spot/export.md`.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde_json::Value as Json;
use spot_api_services::MetaResources;
use spot_api_services::resources::export::{
    self as helpers, FEED_PAGE, Head, Limiter, Scan, Settings, exportable, position_of,
    position_values, scan_changes, spec_from_text, spec_text,
};
use spot_bulk_data::{
    BackendPageReader, BulkCoordinator, BulkError, BulkLimits, BulkResult, ChildTable, Chunk,
    Cursor, CursorBody, CursorKey, EXPORT_ALIAS, ExportAuditEvent, ExportAuditSink,
    ExportAuthority, ExportLease, ExportPhase, ExportPlan, ExportSpec, ExportStream, FieldSet,
    Format, NoopTelemetry, OrderKey, PageReader, StreamContext, format,
};
use spot_core::Clock;
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_field_guard::{
    AuditRecord as GuardAudit, Channel, FieldAccessRecord, FieldGuard, MetaProtection, Offer,
    TraceSink,
};
use spot_http::api::Ports;
use spot_http::api::codec::MetaLookup;
use spot_http::api::filter::parse_filter;
use spot_http::api::ports::export::{
    BulkPort, BulkResult as PortResult, ExportChunkOut, ExportFormatIn, ExportPartOut,
    ExportStartIn, ExtractIn, ExtractPageOut, MAX_TOKEN,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes;
use spot_http::api::service::{ApiError, BoxFuture, MetaService, RequestCtx};
use spot_permissions::{PermissionEngine, Principal, Right, TENANT_ADMINISTRATOR};
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, ChangeFeed, ChangeOp, FeedPosition, Name, Nulls, OrderDir, RecordId, Value,
};
use tokio::sync::watch;

use crate::audit::PathClass;
use crate::live::{Served, SiteLive};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the framework's own services (they use 100 to 900).
pub const ORDER: u32 = 1010;

/// The audit target of this service.
const AUDIT_TARGET: &str = "spot::audit::bulk_export";

/// The tags that keep an export cursor and a watermark apart.
const EXPORT_TAG: &str = "bulk-export-v1";
const MARK_TAG: &str = "bulk-watermark-v1";

/// The extraction epoch every watermark carries. A restore of the feed starts a new feed
/// generation, which the feed itself refuses as `ResyncRequired` (`410`).
const EPOCH: u64 = 1;

/// The longest normalised request a cursor can carry (the cursor is at most 2,048 characters).
const MAX_SPEC_BYTES: usize = 900;

fn limits_of(settings: &Settings) -> BulkLimits {
    BulkLimits {
        rows_per_chunk: settings.rows_per_chunk,
        bytes_per_chunk: settings.bytes_per_chunk,
        max_concurrent_per_tenant: settings.max_concurrent_per_site,
        max_concurrent_per_user: settings.max_concurrent_per_user,
        rows_per_second: settings.rows_per_second,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable).detail("retry_after", Json::from(5))
}

fn denied() -> ApiError {
    ApiError::from_code(Code::PermissionDenied)
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

fn cursor_invalid() -> ApiError {
    helpers::bulk_error("bulk.cursor_invalid", "", 1)
}

fn api(error: &BulkError) -> ApiError {
    if error.code() == "bulk.storage" {
        tracing::warn!(code = error.code(), "a bulk export read failed");
    }
    helpers::bulk_error(error.code(), error.message(), 1)
}

/// Concurrency and rate for one site, in this process.
struct Coordinator(Arc<Limiter>);

struct Slot {
    limiter: Arc<Limiter>,
    user: String,
    budget: u64,
}

impl BulkCoordinator for Coordinator {
    fn acquire(
        &self,
        _tenant: &str,
        user: &str,
        limits: &BulkLimits,
    ) -> BulkResult<Box<dyn ExportLease>> {
        if !self.0.acquire(
            user,
            limits.max_concurrent_per_tenant,
            limits.max_concurrent_per_user,
        ) {
            return Err(BulkError::limit("too many exports are running"));
        }
        Ok(Box::new(Slot {
            limiter: Arc::clone(&self.0),
            user: user.to_owned(),
            budget: limits.rows_per_second,
        }))
    }
}

impl ExportLease for Slot {
    fn take_rows(&self, rows: u64) -> BulkResult<()> {
        if self.limiter.take_rows(rows, self.budget) {
            Ok(())
        } else {
            Err(BulkError::limit("the rows-per-second budget is used up"))
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.limiter.release(&self.user);
    }
}

/// The DocType right `export`, from the permission snapshot served now.
struct SnapshotAuthority(Arc<PermissionEngine>);

impl ExportAuthority for SnapshotAuthority {
    fn may_export(&self, principal: &Principal, doctype: &str) -> bool {
        // An HTTP caller is never the platform itself; an unresolved caller has no assignment.
        if principal.is_system {
            return false;
        }
        let grants = self.0.snapshot().grants();
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
                            && g.rights.contains(Right::Export)
                    })
            })
    }
}

/// The guard's sinks: a record names who, what and through which door, never a value.
struct GuardSinks;

impl TraceSink for GuardSinks {
    fn offer(&self, record: FieldAccessRecord) -> Offer {
        tracing::info!(
            target: "spot::audit::field_access",
            who = %record.who,
            doctype = %record.doctype,
            field = %record.field,
            via = record.via.as_str(),
            authorised = record.authorised,
            "field access"
        );
        Offer::Accepted
    }
}

impl spot_field_guard::AuditSink for GuardSinks {
    fn offer(&self, _record: GuardAudit) -> Offer {
        tracing::info!(target: "spot::audit::field_guard", "field guard record");
        Offer::Accepted
    }
}

const OUTCOME_HANDOFF: u8 = 0;
const OUTCOME_FINISHED: u8 = 1;
const OUTCOME_FAILED: u8 = 2;

/// The audit sink of one request's stream. A request that hands a cursor on is not a
/// cancellation (the next request continues it), so the stream's own `Cancel` record is turned
/// into what the host knows happened.
struct RequestAudit {
    site: String,
    first: bool,
    outcome: AtomicU8,
    code: Mutex<&'static str>,
}

impl RequestAudit {
    fn set(&self, outcome: u8) {
        self.outcome.store(outcome, Ordering::Release);
    }
}

impl ExportAuditSink for RequestAudit {
    fn record(&self, event: &ExportAuditEvent) {
        let outcome = self.outcome.load(Ordering::Acquire);
        let action = match (event.phase, outcome) {
            (ExportPhase::Start, _) if self.first => "started",
            (ExportPhase::Finish, _) | (ExportPhase::Cancel, OUTCOME_FINISHED) => "finished",
            (ExportPhase::Cancel, OUTCOME_FAILED) => "failed",
            _ => return,
        };
        tracing::info!(
            target: AUDIT_TARGET,
            site = %self.site,
            action,
            who = %event.who,
            doctype = %event.doctype,
            columns = event.columns.len(),
            filter = %event.filter_digest,
            plan = %event.plan_digest,
            rows = event.row_count,
            code = if outcome == OUTCOME_FAILED { *lock(&self.code) } else { "" },
            "bulk export"
        );
    }
}

/// Everything derived from one served snapshot: rebuilt when the snapshot is swapped.
struct Prepared {
    served: Arc<Served>,
    catalog: Arc<Catalog>,
    guard: FieldGuard,
    protection: Arc<MetaProtection>,
    authority: SnapshotAuthority,
    metas: MetaResources,
}

struct Caller {
    user: String,
    principal: Principal,
}

struct Shared {
    site: String,
    live: Arc<SiteLive>,
    backend: PgBackend,
    clock: Arc<dyn Clock>,
    settings: Settings,
    export_key: CursorKey,
    mark_key: CursorKey,
    limiter: Arc<Limiter>,
    head: Head,
    prepared: Mutex<Option<Arc<Prepared>>>,
    ready: AtomicBool,
}

impl Shared {
    // ---- the caller and the snapshot ----

    async fn caller(&self, ctx: &RequestCtx) -> PortResult<Caller> {
        if !self.ready.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        if ctx.site.as_str() != self.site {
            return Err(denied());
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if auth.is_restricted(&who) {
            return Err(denied());
        }
        let principals = self.live.principals().ok_or_else(unavailable)?;
        let principal = DocPrincipals::resolve(&*principals, &who.user_id).await;
        Ok(Caller {
            user: who.user_id.to_string(),
            principal,
        })
    }

    fn prepared(&self) -> PortResult<Arc<Prepared>> {
        let served = self.live.served().ok_or_else(unavailable)?;
        let mut cached = lock(&self.prepared);
        if let Some(p) = cached.as_ref()
            && Arc::ptr_eq(&p.served, &served)
        {
            return Ok(Arc::clone(p));
        }
        let catalog = crate::schema::catalog(&served.meta).map_err(|_| unavailable())?;
        let protection = Arc::new(MetaProtection::new(&served.meta));
        let guard = FieldGuard::new(
            Arc::clone(&served.decision),
            Arc::clone(&protection) as Arc<dyn spot_field_guard::ProtectionSource>,
            Arc::new(GuardSinks),
            Arc::new(GuardSinks),
            Arc::clone(&self.clock),
        );
        let prepared = Arc::new(Prepared {
            catalog: Arc::new(catalog),
            guard,
            protection,
            authority: SnapshotAuthority(Arc::clone(&served.decision)),
            metas: MetaResources::new(Arc::clone(&served.meta)),
            served,
        });
        *cached = Some(Arc::clone(&prepared));
        Ok(prepared)
    }

    // ---- the request and its plan ----

    /// The checked plan of `request` for `caller`, with the metadata it was built from.
    async fn plan(
        &self,
        ctx: &RequestCtx,
        prepared: &Prepared,
        caller: &Caller,
        request: &ExportStartIn,
    ) -> PortResult<ExportPlan> {
        // The right comes first: a caller without it learns nothing else, not even whether the
        // DocType or a column exists.
        if !prepared
            .authority
            .may_export(&caller.principal, &request.doctype)
        {
            return Err(denied());
        }
        let def = prepared
            .served
            .meta
            .doctype(&request.doctype)
            .filter(|d| !d.is_child)
            .ok_or_else(|| ApiError::from_code(Code::MetaUnknownDoctype))?;
        let meta = prepared.metas.doctype_meta(ctx, &request.doctype).await?;
        let (fields, secret) = exportable(&def);

        let columns = if request.columns.is_empty() {
            // The default is every column the caller may export: asked one by one, so a
            // refused column leaves the default instead of failing it.
            fields
                .iter()
                .filter(|f| {
                    prepared
                        .guard
                        .columns(
                            Channel::Export,
                            &caller.principal,
                            &request.doctype,
                            std::slice::from_ref(f),
                        )
                        .is_ok()
                })
                .cloned()
                .collect()
        } else {
            helpers::check_columns(&request.columns, &fields, &secret)?;
            request.columns.clone()
        };
        if columns.is_empty() {
            return Err(invalid("The caller may export no column of this DocType."));
        }
        helpers::check_rows(request, self.settings.max_rows_cap)?;

        let alias = Name::new(EXPORT_ALIAS).map_err(|_| ApiError::from_code(Code::HttpInternal))?;
        let mut used: Vec<String> = request.order.iter().map(|o| o.field.clone()).collect();
        let filter = match &request.filter {
            Some(json) => {
                let ast = parse_filter(json, &meta, MetaLookup::none())?;
                used.extend(helpers::filter_fields(&ast));
                Some(helpers::filter_expr(&ast, &meta, &alias)?)
            }
            None => None,
        };
        helpers::check_secrets(&used, &secret)?;
        let order = helpers::order_parts(&request.order, &meta)?
            .into_iter()
            .map(|(field, desc, nulls_first)| OrderKey {
                field,
                dir: if desc { OrderDir::Desc } else { OrderDir::Asc },
                nulls: if nulls_first {
                    Nulls::First
                } else {
                    Nulls::Last
                },
            })
            .collect();
        let mut set = FieldSet::new(
            &request.doctype,
            "created_by",
            &fields.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        if request.include_children {
            for (table, child, columns) in helpers::child_tables(&prepared.served.meta, &def) {
                let columns: Vec<&str> = columns.iter().map(String::as_str).collect();
                set = set.with_child(ChildTable::new(&table, &child, &columns));
            }
        }
        let spec = ExportSpec {
            doctype: request.doctype.clone(),
            columns,
            filter,
            order,
            format: match request.format {
                ExportFormatIn::Csv => Format::Csv,
                ExportFormatIn::Ndjson => Format::Ndjson,
            },
            max_rows: None,
            include_children: request.include_children,
            harden_csv: true,
        };
        ExportPlan::build(
            &spec,
            &caller.principal,
            &prepared.guard,
            &prepared.authority,
            &set,
            &*prepared.protection,
        )
        .map_err(|e| api(&e))
    }

    fn stream_context(
        &self,
        prepared: &Prepared,
        caller: &Caller,
        audit: Arc<RequestAudit>,
        limits: BulkLimits,
    ) -> StreamContext {
        StreamContext {
            reader: Arc::new(BackendPageReader::new(
                self.backend.clone(),
                Arc::clone(&prepared.catalog),
            )),
            key: self.export_key.clone(),
            principal: caller.user.clone(),
            tenant: self.site.clone(),
            limits,
            coordinator: Arc::new(Coordinator(Arc::clone(&self.limiter))),
            telemetry: Arc::new(NoopTelemetry),
            audit,
        }
    }

    /// One refused request, audited with its reason code and nothing else.
    fn refuse(&self, action: &str, who: &str, doctype: &str, error: &ApiError) {
        tracing::info!(
            target: AUDIT_TARGET,
            site = %self.site,
            action,
            who,
            doctype,
            code = %error.code.as_str(),
            "bulk export refused"
        );
    }
}

/// What a decoded export cursor carries.
struct Resume {
    spec: ExportStartIn,
    inner: String,
    mark: FeedPosition,
    delivered: u64,
}

impl Shared {
    /// Signs a body that names `tag`, the site, `middle`, and a feed position.
    fn seal(
        &self,
        key: &CursorKey,
        user: &str,
        rows: u64,
        tag: &str,
        middle: [String; 2],
        at: FeedPosition,
    ) -> PortResult<String> {
        let [generation, seq] = position_values(at).ok_or_else(unavailable)?;
        let [first, second] = middle;
        let keys = [tag.to_owned(), self.site.clone(), first, second]
            .into_iter()
            .map(Value::Text)
            .chain([generation, seq])
            .collect();
        let body = CursorBody {
            plan_digest: [0; 32],
            principal: user.to_owned(),
            part: 0,
            rows,
            keys,
        };
        let token = Cursor::encode(key, &body).map_err(|e| api(&e))?;
        if token.as_str().len() > MAX_TOKEN {
            return Err(invalid(
                "The request is too large to be resumed: use fewer columns, a smaller filter or a shorter sort field.",
            ));
        }
        Ok(token.as_str().to_owned())
    }

    /// Verifies a token: signature, caller, tag and site. Every failure is the same answer.
    fn open(
        &self,
        key: &CursorKey,
        user: &str,
        tag: &str,
        token: &str,
    ) -> PortResult<(u64, [String; 2], FeedPosition)> {
        let body = Cursor::from_token(token)
            .decode(key)
            .map_err(|_| cursor_invalid())?;
        let [
            Value::Text(found),
            Value::Text(site),
            Value::Text(first),
            Value::Text(second),
            generation,
            seq,
        ] = body.keys.as_slice()
        else {
            return Err(cursor_invalid());
        };
        if body.principal != user || found != tag || site != &self.site {
            return Err(cursor_invalid());
        }
        let at = position_of(generation, seq).ok_or_else(cursor_invalid)?;
        Ok((body.rows, [first.clone(), second.clone()], at))
    }

    fn seal_cursor(
        &self,
        user: &str,
        spec: &ExportStartIn,
        inner: &Cursor,
        mark: FeedPosition,
        delivered: u64,
    ) -> PortResult<String> {
        let middle = [spec_text(spec), inner.as_str().to_owned()];
        self.seal(&self.export_key, user, delivered, EXPORT_TAG, middle, mark)
    }

    fn open_cursor(&self, user: &str, token: &str) -> PortResult<Resume> {
        let (delivered, [spec, inner], mark) =
            self.open(&self.export_key, user, EXPORT_TAG, token)?;
        Ok(Resume {
            spec: spec_from_text(&spec).ok_or_else(cursor_invalid)?,
            inner,
            mark,
            delivered,
        })
    }

    fn seal_mark(&self, user: &str, doctype: &str, at: FeedPosition) -> PortResult<String> {
        let middle = [doctype.to_owned(), String::new()];
        self.seal(&self.mark_key, user, EPOCH, MARK_TAG, middle, at)
    }

    fn open_mark(&self, user: &str, doctype: &str, token: &str) -> PortResult<FeedPosition> {
        let (epoch, [bound, _], at) = self.open(&self.mark_key, user, MARK_TAG, token)?;
        if epoch != EPOCH || bound != doctype {
            return Err(cursor_invalid());
        }
        Ok(at)
    }
}

impl Shared {
    /// Runs one chunk of `plan`: the first (`resume` is `None`) or the one after `resume`.
    #[allow(clippy::too_many_arguments)]
    async fn chunk(
        &self,
        prepared: &Prepared,
        caller: &Caller,
        request: &ExportStartIn,
        plan: ExportPlan,
        resume: Option<&Resume>,
        mark: FeedPosition,
    ) -> PortResult<ExportChunkOut> {
        let delivered = resume.map_or(0, |r| r.delivered);
        let mut limits = limits_of(&self.settings);
        // A row cap is the host's: the crate would fail the export, the contract stops it.
        let remaining = request.max_rows.map(|cap| cap.saturating_sub(delivered));
        if let Some(left) = remaining {
            limits.rows_per_chunk = u32::try_from(left)
                .unwrap_or(u32::MAX)
                .min(limits.rows_per_chunk);
        }
        let watermark = self.seal_mark(&caller.user, &request.doctype, mark)?;
        let columns = plan.column_names(&spot_bulk_data::Part::Parent);
        let header = format::header(&columns, plan.format);
        let audit = Arc::new(RequestAudit {
            site: self.site.clone(),
            first: resume.is_none(),
            outcome: AtomicU8::new(OUTCOME_HANDOFF),
            code: Mutex::new(""),
        });
        let context = self.stream_context(prepared, caller, Arc::clone(&audit), limits);
        let plan = Arc::new(plan);
        let mut stream = match resume {
            None => ExportStream::start(Arc::clone(&plan), context),
            Some(r) => {
                ExportStream::resume(Arc::clone(&plan), context, &Cursor::from_token(&r.inner))
            }
        }
        .map_err(|e| api(&e))?;
        let first = if remaining == Some(0) {
            None
        } else {
            match stream.next_chunk().await {
                Ok(chunk) => chunk,
                Err(e) => {
                    *lock(&audit.code) = e.code();
                    audit.set(OUTCOME_FAILED);
                    return Err(api(&e));
                }
            }
        };
        let rows_total = stream.rows();
        let capped = request.max_rows.is_some_and(|cap| rows_total >= cap);
        let done = first.is_none() || capped;
        if done {
            audit.set(OUTCOME_FINISHED);
        }
        let cursor = if done {
            None
        } else {
            let inner = stream.cursor().map_err(|e| api(&e))?;
            Some(self.seal_cursor(&caller.user, request, &inner, mark, rows_total)?)
        };
        let (parts, rows) = match first {
            Some(Chunk { part, bytes, rows }) => (
                vec![ExportPartOut {
                    name: match part {
                        spot_bulk_data::Part::Parent => request.doctype.clone(),
                        spot_bulk_data::Part::Child(table) => table,
                    },
                    bytes,
                }],
                u64::try_from(rows).unwrap_or(u64::MAX),
            ),
            // The first request of an empty export still answers the header.
            None if resume.is_none() && plan.format == Format::Csv => (
                vec![ExportPartOut {
                    name: request.doctype.clone(),
                    bytes: header,
                }],
                0,
            ),
            None => (Vec::new(), 0),
        };
        drop(stream);
        Ok(ExportChunkOut {
            format: request.format,
            multipart: request.include_children,
            parts,
            cursor,
            rows,
            watermark,
        })
    }

    async fn start(&self, ctx: &RequestCtx, request: ExportStartIn) -> PortResult<ExportChunkOut> {
        let caller = self.caller(ctx).await?;
        let doctype = request.doctype.clone();
        let result = async {
            if spec_text(&request).len() > MAX_SPEC_BYTES {
                return Err(invalid(
                    "The request is too large to be resumed: use fewer columns or a smaller filter.",
                ));
            }
            let prepared = self.prepared()?;
            let plan = self.plan(ctx, &prepared, &caller, &request).await?;
            let mark = self.head.advance(&self.backend).await;
            self.chunk(&prepared, &caller, &request, plan, None, mark).await
        }
        .await;
        if let Err(error) = &result {
            self.refuse("start", &caller.user, &doctype, error);
        }
        result
    }

    async fn next(&self, ctx: &RequestCtx, cursor: &str) -> PortResult<ExportChunkOut> {
        let caller = self.caller(ctx).await?;
        let result = async {
            let resume = self.open_cursor(&caller.user, cursor)?;
            let prepared = self.prepared()?;
            let plan = self.plan(ctx, &prepared, &caller, &resume.spec).await?;
            let request = resume.spec.clone();
            self.chunk(
                &prepared,
                &caller,
                &request,
                plan,
                Some(&resume),
                resume.mark,
            )
            .await
        }
        .await;
        if let Err(error) = &result {
            self.refuse("next", &caller.user, "", error);
        }
        result
    }
}

impl Shared {
    async fn extract(&self, ctx: &RequestCtx, request: ExtractIn) -> PortResult<ExtractPageOut> {
        let caller = self.caller(ctx).await?;
        let result = self.extract_page(ctx, &caller, &request).await;
        if let Err(error) = &result {
            self.refuse("extract", &caller.user, &request.doctype, error);
        }
        result
    }

    async fn extract_page(
        &self,
        ctx: &RequestCtx,
        caller: &Caller,
        request: &ExtractIn,
    ) -> PortResult<ExtractPageOut> {
        let since = self.open_mark(&caller.user, &request.doctype, &request.watermark)?;
        let prepared = self.prepared()?;
        let plan = self
            .plan(
                ctx,
                &prepared,
                caller,
                &helpers::extract_request(&request.doctype, &request.columns),
            )
            .await?;
        let meta = prepared.metas.doctype_meta(ctx, &request.doctype).await?;
        let limits = limits_of(&self.settings);
        let lease = Coordinator(Arc::clone(&self.limiter))
            .acquire(&self.site, &caller.user, &limits)
            .map_err(|e| api(&e))?;
        tracing::info!(
            target: AUDIT_TARGET, site = %self.site, action = "started", who = %caller.user,
            doctype = %request.doctype, columns = plan.columns.len(), kind = "extract",
            "bulk extract"
        );

        // The feed is read a bounded distance: enough pages for `limit` documents, never more
        // than the scan bound, whatever the feed holds for other DocTypes.
        let Scan {
            seen,
            after,
            at_end,
        } = scan_changes(&self.backend, since, &request.doctype, request.limit)
            .await
            .map_err(|e| api(&BulkError::from(e)))?;

        let reader = BackendPageReader::new(self.backend.clone(), Arc::clone(&prepared.catalog));
        let wanted: Vec<RecordId> = seen
            .iter()
            .filter(|(_, (op, _))| *op != ChangeOp::Delete)
            .map(|(id, _)| *id)
            .collect();
        let mut upserts = Vec::new();
        let mut found = BTreeSet::new();
        let names = plan.column_names(&spot_bulk_data::Part::Parent);
        let id_at = plan
            .parent_index("id")
            .ok_or_else(|| ApiError::from_code(Code::HttpInternal))?;
        if !plan.read_none {
            for ids in wanted.chunks(FEED_PAGE as usize) {
                let rows = reader
                    .read(&plan.parent_query_by_ids(ids))
                    .await
                    .map_err(|e| api(&BulkError::from(e)))?;
                lease
                    .take_rows(rows.rows.len() as u64)
                    .map_err(|e| api(&e))?;
                for row in &rows.rows {
                    let Some(Value::Id(id)) = row.get(id_at) else {
                        continue;
                    };
                    let version = seen.get(id).map_or(0, |(_, v)| *v);
                    found.insert(*id);
                    upserts.push(helpers::extract_row(
                        &id.to_string(),
                        version,
                        &names,
                        &plan.parent_export_values(row),
                        &meta,
                    ));
                }
            }
        }
        // A document the feed says changed that is now gone, or no longer visible to the
        // caller, is a tombstone: never mere absence.
        let deletes: Vec<String> = seen
            .iter()
            .filter(|(id, (op, _))| *op == ChangeOp::Delete || !found.contains(*id))
            .map(|(id, _)| id.to_string())
            .collect();
        tracing::info!(
            target: AUDIT_TARGET, site = %self.site, action = "finished", who = %caller.user,
            doctype = %request.doctype, rows = upserts.len(), deleted = deletes.len(),
            kind = "extract", "bulk extract"
        );
        Ok(ExtractPageOut {
            upserts,
            deletes,
            new_watermark: self.seal_mark(&caller.user, &request.doctype, after)?,
            has_more: !at_end,
        })
    }
}

/// The bulk port of one site, as `spot-http` mounts it.
struct Bulk(Arc<Shared>);

impl BulkPort for Bulk {
    fn start<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        spec: ExportStartIn,
    ) -> BoxFuture<'a, PortResult<ExportChunkOut>> {
        Box::pin(async move { self.0.start(ctx, spec).await })
    }

    fn next<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        cursor: &'a str,
    ) -> BoxFuture<'a, PortResult<ExportChunkOut>> {
        Box::pin(async move { self.0.next(ctx, cursor).await })
    }

    fn extract<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: ExtractIn,
    ) -> BoxFuture<'a, PortResult<ExtractPageOut>> {
        Box::pin(async move { self.0.extract(ctx, request).await })
    }
}

struct BulkService {
    shared: Arc<Shared>,
}

impl Service for BulkService {
    fn name(&self) -> &'static str {
        "export"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.shared
                .live
                .served()
                .ok_or_else(|| "no metadata is served".to_owned())?;
            self.shared
                .backend
                .read_changes(FeedPosition::START, 1)
                .await
                .map_err(|e| format!("the change feed cannot be read: {}", e.kind.code()))?;
            self.shared.ready.store(true, Ordering::Release);
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            self.shared.ready.store(false, Ordering::Release);
            Ok(())
        })
    }
}

/// The ports of the sites built so far, by site name: a reload builds the site again without
/// building the service again, and its routes must reach the same port.
fn built() -> &'static Mutex<HashMap<String, Arc<Bulk>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<Bulk>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

fn fresh_key() -> CursorKey {
    CursorKey::new(
        spot_sessions::model::SaltKey::generate()
            .as_bytes()
            .to_vec(),
    )
}

fn build(ctx: &mut SiteCtx<'_>, settings: Settings) -> Result<Arc<Bulk>> {
    let env = *ctx.env()?;
    let clock = Arc::clone(ctx.clock());
    let shared = Arc::new(Shared {
        site: env.site.site.clone(),
        live: Arc::clone(env.live),
        backend: env.backend.clone(),
        limiter: Limiter::new(Arc::clone(&clock)),
        clock,
        settings,
        export_key: fresh_key(),
        mark_key: fresh_key(),
        head: Head::default(),
        prepared: Mutex::new(None),
        ready: AtomicBool::new(false),
    });
    ctx.add(Arc::new(BulkService {
        shared: Arc::clone(&shared),
    }));
    Ok(Arc::new(Bulk(shared)))
}

/// Registers bulk export: its service, and its routes on the site's router.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("export")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.export.{key}: out of range")))?;
    if !settings.enabled {
        return Ok(());
    }
    let site = ctx.env()?.site.site.clone();
    let port = if ctx.assembling() {
        let port = build(ctx, settings)?;
        lock(built()).insert(site, Arc::clone(&port));
        port
    } else {
        match lock(built()).get(&site).cloned() {
            Some(port) => port,
            None => return Ok(()),
        }
    };
    let mut ports = Ports::new();
    ports.provide::<dyn BulkPort>(port);
    // The group is mounted here, not by the route registry (see `routes/export.rs`). The host
    // authenticates every request before the router sees it, and the port authenticates again.
    let mounted = routes::mount(&[routes::export::registration()], &ports, &[]);
    let router = mounted.authenticated;
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the export routes were not mounted"
        );
    }
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}
