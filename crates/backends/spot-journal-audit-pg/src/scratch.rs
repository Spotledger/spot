//! The scratch side: a throwaway schema the rebuild applies into, dropped whatever happens.
//!
//! [`PgScratchSide::create`] creates one uniquely named schema in the tenant's own database
//! (`spot_scratch_<tenant>_<run>`, both parts validated as storage names, so no user text reaches
//! an identifier) with the catalogue's tables, exactly as `prepare` creates them
//! ([`crate::ddl`]). The rebuild applies its images into it through [`ApplySink`]; the side then
//! answers `summaries()` and `fingerprints()` from the same tables the live side reads, by the
//! same code ([`crate::pager`]), so the two sides are comparable record for record.
//!
//! # The lifecycle
//!
//! * **Created** once, in one transaction (`CREATE SCHEMA`, its comment, every table). A failure
//!   rolls the whole thing back, so a half-created schema never exists.
//! * **Discarded** by [`ScratchSide::discard`], and again by `Drop` if the caller forgot: the drop
//!   is a `DROP SCHEMA … CASCADE` sent through the held session, so a success, a refusal and an
//!   error path all end with no schema behind. `Drop` is the guard, not a best-effort call at the
//!   end of a happy path; it runs during an unwind too.
//! * **Swept** by [`PgScratchSide::sweep`] when the process died before `Drop` could run: the
//!   schema's own comment carries the run and the creation instant (both arguments, because the
//!   library never reads a clock), so a sweep older than a caller-chosen instant finds exactly the
//!   leftovers of this tenant and drops them.
//!
//! Each image is committed as it arrives: the scratch is private to the run and nobody reads it
//! before the rebuild finished, so there is no half-rebuilt database on show. [`ApplySink::commit`]
//! therefore records nothing and [`ApplySink::abort`] empties the tables.

use std::sync::Arc;
use std::time::Duration;

use spot_events_contract::envelope::Position as FeedPosition;
use spot_journal_audit::{DerivedEntities, Fingerprint};
use spot_journal_audit_run::{ScratchSide, Summaries};
use spot_journal_rebuild::{
    AppliedAudit, AppliedDelete, AppliedImage, ApplySink, PlanSummary, SinkError, SinkFuture,
};
use spot_pg_bridge::{BoxFuture, Bridge, PooledConn};
use spot_storage_contract::schema::system;
use spot_storage_contract::{Catalog, EntityDef, EntityKind, FieldType, Name};

use crate::ddl::{create_table, literal, quote};
use crate::error::{
    PgSideError, image_invalid, options_invalid, query_failed, scratch_invalid, session_lost,
    sweep_failed,
};
use crate::live::check_tenant;
use crate::pager::{Pages, fingerprints as read_fingerprints, page as read_page};
use crate::session::{HeadReport, Request, Requests, Session, next_request, run_blocking};
use crate::sql::{
    FingerprintQuery, Scope, SharedScope, delete_where, insert_child, truncate_all, upsert_document,
};

/// The prefix of every scratch schema this crate creates.
pub const SCHEMA_PREFIX: &str = "spot_scratch_";

/// Rows per page when the caller does not choose.
pub const DEFAULT_PAGE_SIZE: i64 = 256;

/// A deadline long enough for any audit; the bridge clamps it to its own `max_deadline`.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// The tables a sweep lists: every schema this crate owns, with its comment.
const SWEEP_SQL: &str = "SELECT nspname::text, coalesce(obj_description(oid, 'pg_namespace'), '') \
     FROM pg_namespace WHERE starts_with(nspname::text, $1::text) ORDER BY nspname";

/// Whether one schema exists.
const EXISTS_SQL: &str = "SELECT 1 FROM pg_namespace WHERE nspname = $1::text";

/// A scratch schema of one tenant, and the rebuild applied into it.
pub struct PgScratchSide {
    tenant: String,
    run: String,
    schema: String,
    catalog: Arc<Catalog>,
    page_size: i64,
    derived: DerivedEntities,
    fingerprints: Vec<FingerprintQuery>,
    plan: SharedScope,
    scope: Arc<Scope>,
    session: Session,
    aborts: usize,
    discarded: bool,
}

impl std::fmt::Debug for PgScratchSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgScratchSide")
            .field("tenant", &self.tenant)
            .field("schema", &self.schema)
            .field("entities", &self.scope.entities().len())
            .field("page_size", &self.scope.page_size())
            .field("discarded", &self.discarded)
            .finish_non_exhaustive()
    }
}

impl PgScratchSide {
    /// Creates the scratch schema of `tenant` for the run `run`, and holds the session that serves
    /// it.
    ///
    /// `created_millis` is recorded in the schema's comment for [`Self::sweep`]: the library never
    /// reads a clock, so the caller passes the instant.
    ///
    /// # Errors
    ///
    /// [`crate::error::TENANT_MISMATCH`] when either name is not a storage name,
    /// [`crate::error::SCRATCH_INVALID`] when the schema name is too long or already exists,
    /// [`crate::error::OPTIONS_INVALID`] when the catalogue declares no document entity, and the
    /// bridge's own refusal when the database cannot be reached.
    pub fn create(
        bridge: Arc<Bridge>,
        tenant: &str,
        catalog: Arc<Catalog>,
        run: &str,
        created_millis: i64,
    ) -> Result<Self, PgSideError> {
        check_tenant(tenant)?;
        Name::new(run).map_err(|_| {
            scratch_invalid("the run id is not a storage name (lowercase letters, digits, `_`)")
        })?;
        let schema = format!("{SCHEMA_PREFIX}{tenant}_{run}");
        if schema.len() > 63 {
            return Err(scratch_invalid(
                "the scratch schema's name is longer than PostgreSQL's identifier limit",
            ));
        }
        let derived = DerivedEntities::none();
        let scope = Arc::new(Scope::new(
            &catalog,
            Some(schema.clone()),
            &derived,
            DEFAULT_PAGE_SIZE,
            Vec::new(),
        )?);
        let shared = SharedScope::new(Arc::clone(&scope));
        let plan = ScratchPlan {
            schema: schema.clone(),
            tenant: tenant.to_owned(),
            run: run.to_owned(),
            created_millis,
            tables: catalog.entities().map(|e| e.name.clone()).collect(),
            scope: shared.clone(),
        };
        let created = plan.clone();
        let (session, ()) =
            Session::open(&bridge, DEFAULT_DEADLINE, move |conn, requests, head| {
                serve(created, conn, requests, head)
            })?;
        Ok(Self {
            tenant: tenant.to_owned(),
            run: run.to_owned(),
            schema,
            catalog,
            page_size: DEFAULT_PAGE_SIZE,
            derived,
            fingerprints: Vec::new(),
            plan: shared,
            scope,
            session,
            aborts: 0,
            discarded: false,
        })
    }

    /// The schema's name.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// The run id the schema was created for.
    #[must_use]
    pub fn run(&self) -> &str {
        &self.run
    }

    /// Whether this side has already dropped its schema.
    #[must_use]
    pub fn is_discarded(&self) -> bool {
        self.discarded
    }

    /// How many times the rebuild aborted into this side.
    #[must_use]
    pub fn abort_count(&self) -> usize {
        self.aborts
    }

    /// How many pages this side has been asked for since it was created.
    #[must_use]
    pub fn pages_read(&self) -> usize {
        self.session.pages_read()
    }

    /// The first failure recorded since the side was created, if any.
    #[must_use]
    pub fn failure(&self) -> Option<PgSideError> {
        self.session.failure()
    }

    /// Flags the entities the catalogue calls derived, which this side holds but the audit skips.
    ///
    /// # Errors
    ///
    /// [`crate::error::OPTIONS_INVALID`] when no document entity is left to read.
    pub fn with_derived(mut self, derived: DerivedEntities) -> Result<Self, PgSideError> {
        let scope = Arc::new(self.read_plan(&derived, &self.fingerprints)?);
        self.plan.set(Arc::clone(&scope));
        self.scope = scope;
        self.derived = derived;
        Ok(self)
    }

    /// Sets the declared financial fingerprints this side answers.
    ///
    /// # Errors
    ///
    /// [`crate::error::OPTIONS_INVALID`] when a declaration is unusable.
    pub fn with_fingerprints(
        mut self,
        queries: Vec<FingerprintQuery>,
    ) -> Result<Self, PgSideError> {
        let scope = Arc::new(self.read_plan(&self.derived, &queries)?);
        self.plan.set(Arc::clone(&scope));
        self.scope = scope;
        self.fingerprints = queries;
        Ok(self)
    }

    /// The read plan of this side's settings.
    fn read_plan(
        &self,
        derived: &DerivedEntities,
        fingerprints: &[FingerprintQuery],
    ) -> Result<Scope, PgSideError> {
        Scope::new(
            &self.catalog,
            Some(self.schema.clone()),
            derived,
            self.page_size,
            fingerprints.to_vec(),
        )
    }

    /// The summaries of `tenant`'s scratch tables, sorted by entity then id.
    ///
    /// # Errors
    ///
    /// [`crate::error::TENANT_MISMATCH`] for another tenant.
    pub fn summaries_for(&self, tenant: &str) -> Result<Summaries<'_>, PgSideError> {
        self.check(tenant)?;
        Ok(Box::new(Pages::new(&self.session, &self.scope)))
    }

    /// The declared fingerprints of `tenant`'s scratch tables.
    ///
    /// # Errors
    ///
    /// As [`Self::summaries_for`], plus [`crate::error::QUERY_FAILED`] when a sum cannot be read.
    pub fn fingerprints_for(&self, tenant: &str) -> Result<Vec<Fingerprint>, PgSideError> {
        self.check(tenant)?;
        self.session.fingerprints()
    }

    fn check(&self, tenant: &str) -> Result<(), PgSideError> {
        if tenant != self.tenant {
            return Err(crate::error::tenant_mismatch("scratch"));
        }
        Ok(())
    }

    /// Drops the schema now, and ends the session.
    ///
    /// # Errors
    ///
    /// [`crate::error::QUERY_FAILED`] when the schema cannot be dropped. The side is marked
    /// discarded either way, because a failure here leaves the sweep as the only remedy and a
    /// second attempt on a closed session could not help.
    pub fn discard_now(&mut self) -> Result<(), PgSideError> {
        if self.discarded {
            return Ok(());
        }
        self.discarded = true;
        let result = self.session.discard_schema();
        self.session.close();
        result
    }

    /// Whether `schema` exists in the bridge's database (a question for tests and operators).
    ///
    /// # Errors
    ///
    /// [`crate::error::SESSION_FAILED`] when the bridge cannot answer.
    pub fn schema_exists(bridge: Arc<Bridge>, schema: &str) -> Result<bool, PgSideError> {
        let schema = schema.to_owned();
        run_blocking(&bridge, Duration::from_secs(30), move |conn| {
            Box::pin(async move {
                let row = conn
                    .query_opt(EXISTS_SQL, &[&schema])
                    .await
                    .map_err(|error| query_failed(&error))?;
                Ok(row.is_some())
            })
        })
    }

    /// Drops the leftover scratch schemas of `tenant` created before `older_than_millis`, and
    /// returns their names.
    ///
    /// A run killed before `Drop` could run leaves its schema behind; the schema's comment holds
    /// the creation instant, so a sweep with an instant past that run's start removes exactly the
    /// leftovers, and a run still in flight (created later) is untouched. Time is an argument: the
    /// library reads no clock.
    ///
    /// # Errors
    ///
    /// [`crate::error::SWEEP_FAILED`] when the leftovers cannot be listed or dropped, and
    /// [`crate::error::TENANT_MISMATCH`] when `tenant` is not a storage name.
    pub fn sweep(
        bridge: Arc<Bridge>,
        tenant: &str,
        older_than_millis: i64,
    ) -> Result<Vec<String>, PgSideError> {
        check_tenant(tenant)?;
        let prefix = format!("{SCHEMA_PREFIX}{tenant}_");
        let tenant = tenant.to_owned();
        run_blocking(&bridge, Duration::from_secs(30), move |conn| {
            Box::pin(async move {
                let rows = conn.query(SWEEP_SQL, &[&prefix]).await.map_err(|error| {
                    sweep_failed(&format!(
                        "the list failed ({})",
                        error
                            .code()
                            .map(|state| state.code().to_owned())
                            .unwrap_or_else(|| "no SQLSTATE".to_owned())
                    ))
                })?;
                let mut dropped = Vec::new();
                for row in &rows {
                    let name: String = row
                        .try_get(0)
                        .map_err(|_| sweep_failed("a schema's name could not be read"))?;
                    let comment: String = row.try_get(1).unwrap_or_default();
                    if let Some((owner, at)) = parse_comment(&comment)
                        && owner == tenant
                        && at < older_than_millis
                    {
                        conn.batch_execute(&format!(
                            "DROP SCHEMA IF EXISTS {} CASCADE",
                            quote(&name)
                        ))
                        .await
                        .map_err(|_| sweep_failed("a leftover schema could not be dropped"))?;
                        dropped.push(name);
                    }
                }
                Ok(dropped)
            })
        })
    }
}

impl Drop for PgScratchSide {
    /// The guard: whatever path brought the side here — success, a refusal, an error, an unwind —
    /// the schema does not survive it.
    fn drop(&mut self) {
        let _ = self.discard_now();
    }
}

impl ApplySink for PgScratchSide {
    fn begin<'a>(&'a mut self, _summary: &'a PlanSummary) -> SinkFuture<'a> {
        // The schema was created with the side, empty; `begin` has nothing to clear.
        Box::pin(std::future::ready(Ok(())))
    }

    fn apply_image(&mut self, image: AppliedImage) -> SinkFuture<'_> {
        let result = self.session.apply_image(image);
        Box::pin(std::future::ready(
            result.map_err(|error| SinkError::new(error.to_string())),
        ))
    }

    fn apply_delete(&mut self, delete: AppliedDelete) -> SinkFuture<'_> {
        let result = self.session.apply_delete(&delete.entity, &delete.id);
        Box::pin(std::future::ready(
            result.map_err(|error| SinkError::new(error.to_string())),
        ))
    }

    fn apply_audit(&mut self, _audit: AppliedAudit) -> SinkFuture<'_> {
        // An audit-class entry is part of the recoverable record (ADR-061 s2) but not of the
        // record compare: the scratch side keeps no audit table, so the entry is noted and
        // dropped (see the crate README).
        Box::pin(std::future::ready(Ok(())))
    }

    fn commit(&mut self, _last_position: FeedPosition) -> SinkFuture<'_> {
        // Each image was committed as it arrived; the scratch is private to the run.
        Box::pin(std::future::ready(Ok(())))
    }

    fn abort<'a>(&'a mut self, _reason: &'a str) -> SinkFuture<'a> {
        self.aborts += 1;
        let result = self.session.truncate();
        Box::pin(std::future::ready(
            result.map_err(|error| SinkError::new(error.to_string())),
        ))
    }
}

impl ScratchSide for PgScratchSide {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn summaries(&self) -> Summaries<'_> {
        match self.summaries_for(self.tenant.as_str()) {
            Ok(pages) => pages,
            Err(error) => {
                self.session.record(&error);
                Box::new(std::iter::empty())
            }
        }
    }

    fn fingerprints(&self) -> Vec<Fingerprint> {
        match self.fingerprints_for(self.tenant.as_str()) {
            Ok(fingerprints) => fingerprints,
            Err(error) => {
                self.session.record(&error);
                Vec::new()
            }
        }
    }

    fn discard(&mut self) {
        if let Err(error) = self.discard_now() {
            self.session.record(&error);
        }
    }
}

/// What the scratch worker serves: its schema and everything derived from the catalogue once.
#[derive(Clone)]
struct ScratchPlan {
    schema: String,
    tenant: String,
    run: String,
    created_millis: i64,
    tables: Vec<Name>,
    scope: SharedScope,
}

/// The worker of the scratch side: create the schema, then serve the rebuild and the questions.
fn serve(
    plan: ScratchPlan,
    conn: PooledConn,
    mut requests: Requests,
    head: HeadReport<()>,
) -> BoxFuture<'static, Result<(), PgSideError>> {
    Box::pin(async move {
        let created = create_schema(&conn, &plan).await;
        let _ = head.send(created.clone());
        created?;
        while let Some(request) = next_request(&mut requests).await {
            match request {
                Request::ApplyImage { image, reply } => {
                    let scope = plan.scope.get();
                    reply.send(apply_image(&conn, &scope, &image).await);
                }
                Request::ApplyDelete { entity, id, reply } => {
                    let scope = plan.scope.get();
                    reply.send(apply_delete(&conn, &scope, &entity, &id).await);
                }
                Request::Truncate { reply } => reply.send(truncate(&conn, &plan).await),
                Request::Discard { reply } => {
                    let result = drop_schema(&conn, &plan.schema).await;
                    reply.send(result);
                    break;
                }
                Request::Page {
                    entity,
                    after,
                    limit,
                    reply,
                } => {
                    let scope = plan.scope.get();
                    let result = match scope.entity(&entity) {
                        Some(found) => {
                            read_page(&conn, &scope, found, after.as_deref(), limit).await
                        }
                        None => Err(options_invalid(
                            "a page was asked for an entity the scope does not read",
                        )),
                    };
                    reply.send(result);
                }
                Request::Fingerprints { reply } => {
                    let scope = plan.scope.get();
                    reply.send(read_fingerprints(&conn, &scope).await);
                }
                Request::Close { reply } => {
                    reply.send(Ok(()));
                    break;
                }
                #[cfg(feature = "testing")]
                Request::Probe { sql, reply } => {
                    let result = match conn.batch_execute(&sql).await {
                        Ok(()) => Ok(String::new()),
                        Err(error) => Ok(error
                            .code()
                            .map(|state| state.code().to_owned())
                            .unwrap_or_default()),
                    };
                    reply.send(result);
                }
            }
        }
        Ok(())
    })
}

/// Creates the schema and every table in one transaction, and records the run in its comment.
async fn create_schema(conn: &PooledConn, plan: &ScratchPlan) -> Result<(), PgSideError> {
    conn.batch_execute("BEGIN")
        .await
        .map_err(|error| session_lost(&error))?;
    let created = async {
        conn.batch_execute(&format!("CREATE SCHEMA {}", quote(&plan.schema)))
            .await?;
        conn.batch_execute(&format!(
            "COMMENT ON SCHEMA {} IS {}",
            quote(&plan.schema),
            literal(&comment(plan))
        ))
        .await?;
        conn.batch_execute(&format!("SET LOCAL search_path = {}", quote(&plan.schema)))
            .await?;
        for table in &plan.tables {
            if let Some(entity) = plan.scope.get().child_entity(table) {
                conn.batch_execute(&create_table(entity)).await?;
            }
        }
        Ok::<(), tokio_postgres::Error>(())
    }
    .await;
    match created {
        Ok(()) => conn
            .batch_execute("COMMIT")
            .await
            .map_err(|error| session_lost(&error)),
        Err(error) => {
            let _ = conn.batch_execute("ROLLBACK").await;
            Err(query_failed(&error))
        }
    }
}

/// The schema's comment: the tenant and run it belongs to and the instant it was created.
fn comment(plan: &ScratchPlan) -> String {
    format!(
        "spot.journal.audit tenant={} run={} created_millis={}",
        plan.tenant, plan.run, plan.created_millis
    )
}

/// The tenant and creation instant in a schema comment, from [`comment`].
fn parse_comment(text: &str) -> Option<(String, i64)> {
    let mut tenant = None;
    let mut at = None;
    for part in text.split_whitespace() {
        if let Some(value) = part.strip_prefix("tenant=") {
            tenant = Some(value.to_owned());
        } else if let Some(value) = part.strip_prefix("created_millis=") {
            at = value.parse::<i64>().ok();
        }
    }
    Some((tenant?, at?))
}

/// `DROP SCHEMA … CASCADE`.
async fn drop_schema(conn: &PooledConn, schema: &str) -> Result<(), PgSideError> {
    conn.batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", quote(schema)))
        .await
        .map_err(|error| query_failed(&error))
}

/// Empties every scratch table.
async fn truncate(conn: &PooledConn, plan: &ScratchPlan) -> Result<(), PgSideError> {
    match truncate_all(&plan.scope.get(), &plan.tables) {
        Some(sql) => conn
            .batch_execute(&sql)
            .await
            .map_err(|error| query_failed(&error)),
        None => Ok(()),
    }
}

/// Stores one image and its child rows, in one transaction.
async fn apply_image(
    conn: &PooledConn,
    scope: &Scope,
    image: &AppliedImage,
) -> Result<(), PgSideError> {
    let entity = document(scope, &image.entity)?;
    let mut params: Vec<Option<String>> = Vec::with_capacity(entity.fields.len() + 2);
    params.push(Some(image.id.clone()));
    params.push(Some(image.version.to_string()));
    for field in &entity.fields {
        params.push(param(&field.ty, image.image.field(field.name.as_str()))?);
    }
    let statement = upsert_document(scope, entity);

    conn.batch_execute("BEGIN")
        .await
        .map_err(|error| session_lost(&error))?;
    let written = write_image(conn, scope, entity, &statement, &params, image).await;
    match written {
        Ok(()) => conn
            .batch_execute("COMMIT")
            .await
            .map_err(|error| session_lost(&error)),
        Err(error) => {
            let _ = conn.batch_execute("ROLLBACK").await;
            Err(error)
        }
    }
}

/// The statements inside [`apply_image`]'s transaction: the record, then its child rows.
async fn write_image(
    conn: &PooledConn,
    scope: &Scope,
    entity: &EntityDef,
    statement: &str,
    params: &[Option<String>],
    image: &AppliedImage,
) -> Result<(), PgSideError> {
    let bound: Vec<Param<'_>> = params
        .iter()
        .map(|value| value as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();
    conn.execute(statement, &bound)
        .await
        .map_err(|error| query_failed(&error))?;
    for table in &entity.children {
        let child = scope.child_entity(&table.entity).ok_or_else(|| {
            options_invalid("a child table names an entity the catalogue does not declare")
        })?;
        let delete: Param<'_> = &image.id;
        conn.execute(&delete_where(scope, &child.name, system::PARENT), &[delete])
            .await
            .map_err(|error| query_failed(&error))?;
        let rows = match image.image.field(table.field.as_str()) {
            Some(serde_json::Value::Array(rows)) => rows.clone(),
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(_) => {
                return Err(image_invalid(
                    "a child table in the image is not an array of rows",
                ));
            }
        };
        for (at, row) in rows.iter().enumerate() {
            insert_child_row(conn, scope, child, table.field.as_str(), &image.id, at, row).await?;
        }
    }
    Ok(())
}

/// One child row of an image.
async fn insert_child_row(
    conn: &PooledConn,
    scope: &Scope,
    child: &EntityDef,
    parent_field: &str,
    parent: &str,
    at: usize,
    row: &serde_json::Value,
) -> Result<(), PgSideError> {
    let serde_json::Value::Object(row) = row else {
        return Err(image_invalid("a child row in the image is not an object"));
    };
    let id = match row.get("id") {
        Some(serde_json::Value::String(id)) => id.clone(),
        _ => {
            return Err(image_invalid(
                "a child row in the image carries no id, which the scratch table needs",
            ));
        }
    };
    let idx = match row.get("idx") {
        Some(serde_json::Value::Number(number)) => number
            .as_i64()
            .ok_or_else(|| image_invalid("a child row's position is not an integer"))?,
        _ => i64::try_from(at + 1)
            .map_err(|_| image_invalid("a child table has more rows than it can index"))?,
    };
    let mut params: Vec<Option<String>> = vec![
        Some(id),
        Some(parent.to_owned()),
        Some(parent_field.to_owned()),
        Some(idx.to_string()),
    ];
    for field in &child.fields {
        params.push(param(&field.ty, row.get(field.name.as_str()))?);
    }
    let bound: Vec<Param<'_>> = params
        .iter()
        .map(|value| value as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();
    conn.execute(&insert_child(scope, child), &bound)
        .await
        .map_err(|error| query_failed(&error))?;
    Ok(())
}

/// Removes one record and its child rows, in one transaction.
async fn apply_delete(
    conn: &PooledConn,
    scope: &Scope,
    entity: &str,
    id: &str,
) -> Result<(), PgSideError> {
    let entity = document(scope, entity)?;
    let statement = delete_where(scope, &entity.name, system::ID);
    conn.batch_execute("BEGIN")
        .await
        .map_err(|error| session_lost(&error))?;
    let removed = async {
        let bound: Param<'_> = &id;
        for table in &entity.children {
            let child = scope.child_entity(&table.entity).ok_or_else(|| {
                options_invalid("a child table names an entity the catalogue does not declare")
            })?;
            conn.execute(&delete_where(scope, &child.name, system::PARENT), &[bound])
                .await
                .map_err(|error| query_failed(&error))?;
        }
        conn.execute(&statement, &[bound])
            .await
            .map_err(|error| query_failed(&error))?;
        Ok::<(), PgSideError>(())
    }
    .await;
    match removed {
        Ok(()) => conn
            .batch_execute("COMMIT")
            .await
            .map_err(|error| session_lost(&error)),
        Err(error) => {
            let _ = conn.batch_execute("ROLLBACK").await;
            Err(error)
        }
    }
}

/// The document entity called `name`, or a refusal: a child table is part of its parent's image
/// and never a record of its own.
fn document<'a>(scope: &'a Scope, name: &str) -> Result<&'a EntityDef, PgSideError> {
    let name = Name::new(name)
        .map_err(|_| image_invalid("an image names an entity that is not a storage name"))?;
    let entity = scope
        .child_entity(&name)
        .ok_or_else(|| image_invalid("an image names an entity the catalogue does not declare"))?;
    match entity.kind {
        EntityKind::Document => Ok(entity),
        EntityKind::Child { .. } => Err(image_invalid(
            "an image names a child table, which is stored inside its parent's image",
        )),
    }
}

/// One body value as the text parameter its write statement binds; the database casts it to the
/// column's own type, so its own parser decides what a value may be.
fn param(ty: &FieldType, value: Option<&serde_json::Value>) -> Result<Option<String>, PgSideError> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match (ty, value) {
        (_, serde_json::Value::Null) => None,
        (FieldType::Bool, serde_json::Value::Bool(flag)) => {
            Some(if *flag { "t" } else { "f" }.to_owned())
        }
        (FieldType::Int, serde_json::Value::Number(number)) => Some(number.to_string()),
        (FieldType::Json, other) => Some(
            serde_json::to_string(other)
                .map_err(|_| image_invalid("a json field cannot be written as JSON"))?,
        ),
        (
            FieldType::Decimal { .. }
            | FieldType::Text { .. }
            | FieldType::Date
            | FieldType::Time
            | FieldType::DateTime
            | FieldType::Link { .. }
            | FieldType::Bytes
            | FieldType::Geo { .. }
            | FieldType::Range { .. },
            serde_json::Value::String(text),
        ) => Some(text.clone()),
        _ => {
            return Err(image_invalid(
                "a body value's shape contradicts its declared field type",
            ));
        }
    })
}

/// A bound parameter of a write statement.
type Param<'a> = &'a (dyn tokio_postgres::types::ToSql + Sync);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_comment_round_trips() {
        let plan = ScratchPlan {
            schema: "spot_scratch_acme_run1".to_owned(),
            tenant: "acme".to_owned(),
            run: "run1".to_owned(),
            created_millis: 1_700_000_000_000,
            tables: Vec::new(),
            scope: SharedScope::new(Arc::new(
                Scope::new(
                    &Arc::new(
                        Catalog::new(
                            1,
                            spot_storage_contract::Collation::new("en")
                                .unwrap_or_else(|e| panic!("{e}")),
                            vec![EntityDef {
                                name: Name::new("invoice").unwrap_or_else(|e| panic!("{e}")),
                                kind: EntityKind::Document,
                                fields: Vec::new(),
                                children: Vec::new(),
                                indexes: Vec::new(),
                                exclusions: Vec::new(),
                                search: None,
                            }],
                        )
                        .unwrap_or_else(|e| panic!("{e}")),
                    ),
                    Some("spot_scratch_acme_run1".to_owned()),
                    &DerivedEntities::none(),
                    10,
                    Vec::new(),
                )
                .unwrap_or_else(|e| panic!("{e}")),
            )),
        };
        let text = comment(&plan);
        assert_eq!(
            parse_comment(&text),
            Some(("acme".to_owned(), 1_700_000_000_000))
        );
        assert_eq!(parse_comment("no comment here"), None);
    }

    #[test]
    fn a_body_value_becomes_the_parameter_its_type_expects() {
        use serde_json::json;
        assert_eq!(
            param(&FieldType::Bool, Some(&json!(true))),
            Ok(Some("t".to_owned()))
        );
        assert_eq!(
            param(&FieldType::Int, Some(&json!(7))),
            Ok(Some("7".to_owned()))
        );
        assert_eq!(
            param(&FieldType::Json, Some(&json!({"a": 1}))),
            Ok(Some("{\"a\":1}".to_owned()))
        );
        assert_eq!(
            param(&FieldType::Text { max_chars: None }, Some(&json!("x"))),
            Ok(Some("x".to_owned()))
        );
        assert_eq!(
            param(&FieldType::Text { max_chars: None }, Some(&json!(null))),
            Ok(None)
        );
        assert_eq!(
            param(&FieldType::Int, Some(&json!("x"))),
            Err(image_invalid(
                "a body value's shape contradicts its declared field type"
            ))
        );
    }
}
