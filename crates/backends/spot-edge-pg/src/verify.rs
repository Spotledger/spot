//! The `edge_verify` job: page a tenant's documents in key order, run the invariant against the
//! edge store, and write a drift report.
//!
//! The run is **read-only over tenant data**: it loads documents and edges and writes only the
//! report tables, never an edge. Memory is bounded: documents are paged per entity in id order
//! and the edges for a page are read in one bounded query; a second pass, also paged, finds
//! edges whose owner document no longer exists.
//!
//! The handler returns [`HandlerResult::Ok`] whether or not the run found drift — a drift report
//! is the point, not a failure — and [`HandlerResult::Retry`] when the store was unreachable.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

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
        }
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
        let mut tx = self.begin().await?;
        let mut report = DriftReport::new(report_key, now);
        let entities: Vec<Name> = self
            .catalog
            .entities()
            .filter(|e| e.kind == EntityKind::Document)
            .map(|e| e.name.clone())
            .collect();
        for entity in &entities {
            self.scan_entity(&mut tx, entity, &mut report).await?;
        }
        for entity in &entities {
            self.scan_orphans(&mut tx, entity, &mut report).await?;
        }
        report.entries.sort_by(|a, b| {
            (&a.edge_type, &a.owner, &a.from, &a.to).cmp(&(&b.edge_type, &b.owner, &b.from, &b.to))
        });
        write_report(&mut tx, &report).await?;
        tx.commit()
            .await
            .map_err(|e| EdgePgError::write(e.explanation))?;
        Ok(report)
    }

    async fn begin(&self) -> Result<PgTransaction> {
        let label = Name::new("edge_verify")
            .map_err(|_| EdgePgError::write("`edge_verify` is not a valid transaction label"))?;
        let options = TxOptions::new(label, Isolation::ReadCommitted);
        self.backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|e| EdgePgError::Unavailable {
                message: e.explanation,
            })
    }

    /// Pages one entity's documents in id order and checks each page against the store.
    async fn scan_entity(
        &self,
        tx: &mut PgTransaction,
        entity: &Name,
        report: &mut DriftReport,
    ) -> Result<()> {
        let mut offset = 0i64;
        loop {
            let ids = self.page_ids(tx, entity, offset).await?;
            if ids.is_empty() {
                break;
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
            let drift = check_invariant(&self.schema, docs.iter(), actual);
            for item in &drift {
                report.entries.push(DriftEntry::from_drift(item)?);
            }
            report.checked_documents += u64::try_from(ids.len()).unwrap_or(0);
            offset += i64::try_from(ids.len()).unwrap_or(0);
            if u32::try_from(ids.len()).unwrap_or(u32::MAX) < self.page {
                break;
            }
        }
        Ok(())
    }

    /// Pages one entity's orphan edges: edges whose owner document no longer exists.
    async fn scan_orphans(
        &self,
        tx: &mut PgTransaction,
        entity: &Name,
        report: &mut DriftReport,
    ) -> Result<()> {
        let table = quote(entity.as_str());
        let sql = format!(
            "SELECT {EDGE_COLUMNS} FROM spot_graph_edge \
             WHERE owner_entity = $1 AND NOT EXISTS \
             (SELECT 1 FROM {table} d WHERE d.id = spot_graph_edge.owner_id) \
             ORDER BY edge_type, owner_id, from_entity, from_id, to_entity, to_id \
             LIMIT $2 OFFSET $3"
        );
        let entity_text = entity.as_str().to_owned();
        let limit = i64::from(self.page);
        let mut offset = 0i64;
        loop {
            let rows = tx
                .raw_query(&sql, &[&entity_text, &limit, &offset], None)
                .await
                .map_err(|e| EdgePgError::write(e.explanation))?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                let edge = row_to_edge(decode_row(row)?)?;
                let extra = Drift::Extra {
                    found: Box::new(edge),
                };
                report.entries.push(DriftEntry::from_drift(&extra)?);
            }
            offset += i64::try_from(rows.len()).unwrap_or(0);
            if u32::try_from(rows.len()).unwrap_or(u32::MAX) < self.page {
                break;
            }
        }
        Ok(())
    }

    async fn page_ids(
        &self,
        tx: &mut PgTransaction,
        entity: &Name,
        offset: i64,
    ) -> Result<Vec<spot_storage_contract::RecordId>> {
        let sql = format!(
            "SELECT id FROM {} ORDER BY id LIMIT $1 OFFSET $2",
            quote(entity.as_str())
        );
        let limit = i64::from(self.page);
        let rows = tx
            .raw_query(&sql, &[&limit, &offset], None)
            .await
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
