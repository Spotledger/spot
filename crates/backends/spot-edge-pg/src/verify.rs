//! The `edge_verify` job: page a tenant's documents in key order, run the invariant against the
//! edge store, and write a drift report.
//!
//! The run is **read-only over tenant data**: it loads documents and edges and writes only the
//! report tables, never an edge. Memory is bounded: documents are paged per entity by keyset
//! (`id > last id`, never an `OFFSET`, so a page costs the same at any depth) and the edges for a
//! page are read in one bounded query; a second pass, also keyset-paged, finds edges whose owner
//! document no longer exists.
//!
//! **A scan never starves the serving path.** Every page runs in its own short repeatable-read
//! transaction (one connection, returned to the pool when the page is done), not one transaction
//! that holds a connection and a snapshot for the whole scan; the run takes one connection at a
//! time, and an optional pause between pages ([`EdgeVerifyJob::with_pause`]) lets the requests
//! that are waiting for the database go first. The only write is the final report transaction.
//!
//! The handler returns [`HandlerResult::Ok`] whether or not the run found drift — a drift report
//! is the point, not a failure — and [`HandlerResult::Retry`] when the store was unreachable.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use spot_app_api::Document;
use spot_edge_sync::{Drift, EdgeSchema, check_invariant};
use spot_jobs::worker::{HandlerResult, JobContext};
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{
    Catalog, DocumentStore, EntityKind, Isolation, Name, RowLock, StorageBackend, Transaction,
    TxOptions,
};
use time::OffsetDateTime;
use tokio_postgres::types::ToSql;
use uuid::Uuid;

use crate::convert::{record_id_of, row_to_edge, uuid_of};
use crate::error::{EdgePgError, Result};
use crate::report::{DriftEntry, DriftReport, write_report};
use crate::schema::quote;
use crate::writer::{EDGE_COLUMNS, decode_row};

/// How many documents one page holds by default.
pub const DEFAULT_PAGE_SIZE: u32 = 500;

/// The boxed future a [`handler`] returns.
pub type VerifyFuture = Pin<Box<dyn Future<Output = spot_jobs::Result<HandlerResult>> + Send>>;

/// The background invariant scan over one tenant.
pub struct EdgeVerifyJob {
    backend: PgBackend,
    catalog: Arc<Catalog>,
    schema: EdgeSchema,
    page: u32,
    pause: Duration,
}

impl EdgeVerifyJob {
    /// A job over `backend` and `catalog`, checking `schema`.
    #[must_use]
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>, schema: EdgeSchema) -> Self {
        Self {
            backend,
            catalog,
            schema,
            page: DEFAULT_PAGE_SIZE,
            pause: Duration::ZERO,
        }
    }

    /// The same job pausing this long between two pages, so a scan of a large tenant leaves the
    /// connection pool and the database to the requests being served (default: no pause).
    #[must_use]
    pub fn with_pause(mut self, pause: Duration) -> Self {
        self.pause = pause;
        self
    }

    /// The same job with a different page size (at least one).
    #[must_use]
    pub fn with_page_size(mut self, page: u32) -> Self {
        self.page = page.max(1);
        self
    }

    /// Runs the scan and writes the report under `report_key`.
    ///
    /// `now` is the instant the run happened; the job reads no clock.
    ///
    /// # Errors
    /// `edge_pg.unavailable` when the store cannot be reached, `edge_pg.write_failed` when the
    /// report cannot be written.
    pub async fn run(&self, now: OffsetDateTime, report_key: &str) -> Result<DriftReport> {
        let mut report = DriftReport::new(report_key, now);
        let entities: Vec<Name> = self
            .catalog
            .entities()
            .filter(|e| e.kind == EntityKind::Document)
            .map(|e| e.name.clone())
            .collect();
        for entity in &entities {
            self.scan_entity(entity, &mut report).await?;
        }
        for entity in &entities {
            self.scan_orphans(entity, &mut report).await?;
        }
        report.entries.sort_by(|a, b| {
            (&a.edge_type, &a.owner, &a.from, &a.to).cmp(&(&b.edge_type, &b.owner, &b.from, &b.to))
        });
        let mut tx = self.begin(Isolation::ReadCommitted).await?;
        write_report(&mut tx, &report).await?;
        tx.commit()
            .await
            .map_err(|e| EdgePgError::write(e.explanation))?;
        Ok(report)
    }

    /// Deletes the report `report_key` (and its entries): a report kept only to drive one repair.
    ///
    /// # Errors
    /// `edge_pg.unavailable` when the store cannot be reached, `edge_pg.write_failed` when the
    /// delete fails.
    pub async fn discard_report(&self, report_key: &str) -> Result<()> {
        let mut tx = self.begin(Isolation::ReadCommitted).await?;
        crate::report::delete_report(&mut tx, report_key).await?;
        tx.commit()
            .await
            .map_err(|e| EdgePgError::write(e.explanation))
    }

    async fn begin(&self, isolation: Isolation) -> Result<PgTransaction> {
        let label = Name::new("edge_verify")
            .map_err(|_| EdgePgError::write("`edge_verify` is not a valid transaction label"))?;
        let options = TxOptions::new(label, isolation);
        let mut tx = self
            .backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|e| EdgePgError::Unavailable {
                message: e.explanation,
            })?;
        // A background scan reads page by page through indexes; it never needs (and the shared
        // memory of a small database must not be spent on) parallel workers.
        tx.raw_query(
            "SELECT set_config('max_parallel_workers_per_gather', '0', true)",
            &[],
            None,
        )
        .await
        .map_err(|e| EdgePgError::Unavailable {
            message: e.explanation,
        })?;
        Ok(tx)
    }

    /// Pages one entity's documents in id order and checks each page against the store. Each
    /// page is read in its own repeatable-read transaction, so the documents and their edges
    /// come from one snapshot and no connection is held between pages.
    async fn scan_entity(&self, entity: &Name, report: &mut DriftReport) -> Result<()> {
        let mut after: Option<spot_storage_contract::RecordId> = None;
        loop {
            let mut tx = self.begin(Isolation::RepeatableRead).await?;
            let page = self.read_page(&mut tx, entity, after).await;
            // Nothing was written: the end of the page is a rollback, whatever its outcome.
            let _ = tx.rollback().await;
            let (ids, docs, actual) = page?;
            let Some(last) = ids.last().copied() else {
                break;
            };
            let drift = check_invariant(&self.schema, docs.iter(), actual);
            for item in &drift {
                report.entries.push(DriftEntry::from_drift(item)?);
            }
            report.checked_documents += u64::try_from(ids.len()).unwrap_or(0);
            if u32::try_from(ids.len()).unwrap_or(u32::MAX) < self.page {
                break;
            }
            after = Some(last);
            self.rest().await;
        }
        Ok(())
    }

    /// One page: the ids after `after`, their documents and the edges they own.
    async fn read_page(
        &self,
        tx: &mut PgTransaction,
        entity: &Name,
        after: Option<spot_storage_contract::RecordId>,
    ) -> Result<(
        Vec<spot_storage_contract::RecordId>,
        Vec<Document>,
        Vec<spot_edge_sync::Edge>,
    )> {
        let ids = self.page_ids(tx, entity, after).await?;
        if ids.is_empty() {
            return Ok((ids, Vec::new(), Vec::new()));
        }
        let mut docs = Vec::with_capacity(ids.len());
        for id in &ids {
            let stored = tx
                .load(entity, *id, RowLock::None)
                .await
                .map_err(|e| EdgePgError::write(e.explanation))?;
            if let Some(stored) = stored {
                docs.push(to_app_document(stored));
            }
        }
        let actual = self.edges_for_owners(tx, entity, &ids).await?;
        Ok((ids, docs, actual))
    }

    /// Pages one entity's orphan edges: edges whose owner document no longer exists. Pages over
    /// the distinct owners the edge table holds, by keyset on `owner_id` through
    /// `spot_graph_edge_owner_idx`, checks only that page's owners against the document table,
    /// and reads the edges of the owners that are gone. Each page is its own transaction. (An
    /// anti-join over the whole entity read both tables in full for every check and, planned as a
    /// parallel hash join, wanted more shared memory than the database could give.)
    async fn scan_orphans(&self, entity: &Name, report: &mut DriftReport) -> Result<()> {
        let table = quote(entity.as_str());
        let owners_first = format!(
            "SELECT o.owner_id, NOT EXISTS (SELECT 1 FROM {table} d WHERE d.id = o.owner_id) \
             FROM (SELECT DISTINCT owner_id FROM spot_graph_edge WHERE owner_entity = $1 \
                   ORDER BY owner_id LIMIT $2) o ORDER BY o.owner_id"
        );
        let owners_next = format!(
            "SELECT o.owner_id, NOT EXISTS (SELECT 1 FROM {table} d WHERE d.id = o.owner_id) \
             FROM (SELECT DISTINCT owner_id FROM spot_graph_edge WHERE owner_entity = $1 \
                   AND owner_id > $3 ORDER BY owner_id LIMIT $2) o ORDER BY o.owner_id"
        );
        let edges_of = format!(
            "SELECT {EDGE_COLUMNS} FROM spot_graph_edge \
             WHERE owner_entity = $1 AND owner_id = ANY($2::uuid[]) \
             ORDER BY edge_type, owner_id, from_entity, from_id, to_entity, to_id"
        );
        let entity_text = entity.as_str().to_owned();
        let limit = i64::from(self.page);
        let mut after: Option<Uuid> = None;
        loop {
            let mut tx = self.begin(Isolation::RepeatableRead).await?;
            let page = self
                .read_orphan_page(
                    &mut tx,
                    &entity_text,
                    limit,
                    after,
                    (&owners_first, &owners_next, &edges_of),
                )
                .await;
            let _ = tx.rollback().await;
            let (owners, last, orphans) = page?;
            for edge in orphans {
                let extra = Drift::Extra {
                    found: Box::new(edge),
                };
                report.entries.push(DriftEntry::from_drift(&extra)?);
            }
            let Some(last) = last else {
                break;
            };
            if u32::try_from(owners).unwrap_or(u32::MAX) < self.page {
                break;
            }
            after = Some(last);
            self.rest().await;
        }
        Ok(())
    }

    /// One page of owners: how many were read, the last of them, and the edges of those whose
    /// document is gone.
    async fn read_orphan_page(
        &self,
        tx: &mut PgTransaction,
        entity_text: &str,
        limit: i64,
        after: Option<Uuid>,
        sql: (&str, &str, &str),
    ) -> Result<(usize, Option<Uuid>, Vec<spot_edge_sync::Edge>)> {
        let failed = |e: spot_storage_contract::StorageError| EdgePgError::write(e.explanation);
        let entity_text = entity_text.to_owned();
        let owners = match after {
            None => tx.raw_query(sql.0, &[&entity_text, &limit], None).await,
            Some(after) => {
                tx.raw_query(sql.1, &[&entity_text, &limit, &after], None)
                    .await
            }
        }
        .map_err(failed)?;
        let mut last = None;
        let mut gone: Vec<Uuid> = Vec::new();
        for row in &owners {
            let id: Uuid = row
                .try_get(0)
                .map_err(|e| EdgePgError::decode(e.to_string()))?;
            let orphan: bool = row
                .try_get(1)
                .map_err(|e| EdgePgError::decode(e.to_string()))?;
            last = Some(id);
            if orphan {
                gone.push(id);
            }
        }
        let mut edges = Vec::new();
        if !gone.is_empty() {
            let rows = tx
                .raw_query(sql.2, &[&entity_text, &gone], None)
                .await
                .map_err(failed)?;
            for row in &rows {
                edges.push(row_to_edge(decode_row(row)?)?);
            }
        }
        Ok((owners.len(), last, edges))
    }

    /// The pause between two pages.
    async fn rest(&self) {
        tokio::time::sleep(self.pause).await;
    }

    async fn page_ids(
        &self,
        tx: &mut PgTransaction,
        entity: &Name,
        after: Option<spot_storage_contract::RecordId>,
    ) -> Result<Vec<spot_storage_contract::RecordId>> {
        let limit = i64::from(self.page);
        let table = quote(entity.as_str());
        let rows = match after {
            None => {
                let sql = format!("SELECT id FROM {table} ORDER BY id LIMIT $1");
                tx.raw_query(&sql, &[&limit], None).await
            }
            Some(after) => {
                let sql = format!("SELECT id FROM {table} WHERE id > $2 ORDER BY id LIMIT $1");
                let after = uuid_of(after);
                tx.raw_query(&sql, &[&limit, &after], None).await
            }
        }
        .map_err(|e| EdgePgError::write(e.explanation))?;
        let mut ids = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: Uuid = row
                .try_get(0)
                .map_err(|e| EdgePgError::decode(e.to_string()))?;
            ids.push(record_id_of(id)?);
        }
        Ok(ids)
    }

    async fn edges_for_owners(
        &self,
        tx: &mut PgTransaction,
        entity: &Name,
        ids: &[spot_storage_contract::RecordId],
    ) -> Result<Vec<spot_edge_sync::Edge>> {
        let sql = format!(
            "SELECT {EDGE_COLUMNS} FROM spot_graph_edge \
             WHERE owner_entity = $1 AND owner_id = ANY($2::uuid[]) \
             ORDER BY edge_type, from_entity, from_id, to_entity, to_id"
        );
        let entity_text = entity.as_str().to_owned();
        let owners: Vec<Uuid> = ids.iter().map(|id| uuid_of(*id)).collect();
        let params: [&(dyn ToSql + Sync); 2] = [&entity_text, &owners];
        let rows = tx
            .raw_query(&sql, &params, None)
            .await
            .map_err(|e| EdgePgError::write(e.explanation))?;
        let mut edges = Vec::with_capacity(rows.len());
        for row in &rows {
            edges.push(row_to_edge(decode_row(row)?)?);
        }
        edges.sort();
        Ok(edges)
    }
}

/// A stored document as the app API document the invariant checker reads.
fn to_app_document(stored: spot_storage_contract::Document) -> Document {
    let data = stored.data;
    let tables = data
        .children
        .into_iter()
        .map(|(name, rows)| {
            let rows = rows
                .into_iter()
                .map(|row| spot_app_api::ChildRow {
                    id: row.id,
                    fields: row.fields,
                })
                .collect();
            (name, rows)
        })
        .collect();
    Document::loaded(
        data.entity,
        data.id,
        stored.version,
        None,
        data.fields,
        tables,
    )
}

/// The `edge_verify` job handler: a closure a [`JobHandlerRegistry`](spot_jobs::worker::JobHandlerRegistry)
/// registers under the name `edge_verify`.
///
/// The job's `args` carry `{"report_key": "..."}`.
pub fn handler(
    job: Arc<EdgeVerifyJob>,
) -> impl Fn(JobContext) -> VerifyFuture + Send + Sync + 'static {
    move |ctx: JobContext| {
        let job = Arc::clone(&job);
        Box::pin(async move {
            let Some(report_key) = ctx
                .job
                .args
                .get("report_key")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
            else {
                return Ok(HandlerResult::Fail(
                    "edge_verify needs a `report_key` argument".to_owned(),
                ));
            };
            match job.run(ctx.clock, &report_key).await {
                Ok(_) => Ok(HandlerResult::Ok),
                Err(error) => Ok(HandlerResult::Retry(error.code().to_owned())),
            }
        })
    }
}
