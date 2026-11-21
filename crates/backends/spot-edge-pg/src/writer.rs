//! [`PgEdgeWriter`]: the [`EdgeWriter`] that writes the `spot_graph_edge` table in the caller's
//! transaction.
//!
//! The writer borrows the caller's [`PgTransaction`], so every add, remove and update commits
//! with the document write that shares the transaction: a document and its edges are never
//! half-written. The statements are the crate's own SQL with every value bound (ADR-45); the
//! table and its index are created by [`prepare`](crate::schema::prepare).
//!
//! [`scan_by_source`](EdgeWriter::scan_by_source) is the bounded read the hot path uses: it
//! reads the edges owned by one record through `spot_graph_edge_owner_idx`, so it never scans the
//! whole table.

use spot_app_api::prelude::SiteId;
use spot_edge_sync::{Edge, EdgeFuture, EdgeResult, EdgeWriter, EdgeWriterFor};
use spot_postgres::PgTransaction;
use spot_storage_contract::NodeRef;
use tokio_postgres::types::ToSql;
use uuid::Uuid;

use crate::convert::{EdgeRow, edge_to_row, row_to_edge, uuid_of};
use crate::error::EdgePgError;

/// Writes an edge whole: a new edge is inserted, an existing one has its properties replaced.
const UPSERT: &str = "INSERT INTO spot_graph_edge \
    (edge_type, owner_entity, owner_id, from_entity, from_id, to_entity, to_id, properties) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
    ON CONFLICT (edge_type, owner_entity, owner_id, from_entity, from_id, to_entity, to_id) \
    DO UPDATE SET properties = EXCLUDED.properties";

/// Removes one edge by its whole identity.
const REMOVE: &str = "DELETE FROM spot_graph_edge WHERE edge_type = $1 AND owner_entity = $2 \
    AND owner_id = $3 AND from_entity = $4 AND from_id = $5 AND to_entity = $6 AND to_id = $7";

/// The `spot_graph_edge` columns in the order [`decode_row`] reads them.
pub(crate) const EDGE_COLUMNS: &str = "edge_type, owner_entity, owner_id, from_entity, from_id, \
    to_entity, to_id, properties";

/// Every edge owned by one record, through `spot_graph_edge_owner_idx`.
const SCAN: &str = "SELECT edge_type, owner_entity, owner_id, from_entity, from_id, to_entity, \
    to_id, properties FROM spot_graph_edge WHERE owner_entity = $1 AND owner_id = $2 \
    ORDER BY edge_type, from_entity, from_id, to_entity, to_id";

/// The [`EdgeWriter`] over the caller's PostgreSQL transaction.
pub struct PgEdgeWriter<'a> {
    tx: &'a mut PgTransaction,
}

impl<'a> PgEdgeWriter<'a> {
    /// A writer on `tx`.
    pub fn new(tx: &'a mut PgTransaction) -> Self {
        Self { tx }
    }

    /// Writes one edge whole (an upsert).
    pub async fn upsert(&mut self, edge: &Edge) -> EdgeResult<()> {
        let row = edge_to_row(edge).map_err(EdgePgError::into_edge_sync)?;
        let params: [&(dyn ToSql + Sync); 8] = [
            &row.edge_type,
            &row.owner_entity,
            &row.owner_id,
            &row.from_entity,
            &row.from_id,
            &row.to_entity,
            &row.to_id,
            &row.properties,
        ];
        self.tx
            .raw_execute(UPSERT, &params)
            .await
            .map_err(|e| EdgePgError::write(e.explanation).into_edge_sync())?;
        Ok(())
    }

    /// Removes one edge by its whole identity.
    pub async fn delete(&mut self, edge: &Edge) -> EdgeResult<()> {
        let row = edge_to_row(edge).map_err(EdgePgError::into_edge_sync)?;
        let params: [&(dyn ToSql + Sync); 7] = [
            &row.edge_type,
            &row.owner_entity,
            &row.owner_id,
            &row.from_entity,
            &row.from_id,
            &row.to_entity,
            &row.to_id,
        ];
        self.tx
            .raw_execute(REMOVE, &params)
            .await
            .map_err(|e| EdgePgError::write(e.explanation).into_edge_sync())?;
        Ok(())
    }

    /// Every edge owned by `source`, in edge-key order.
    pub async fn edges_owned_by(&mut self, source: &NodeRef) -> EdgeResult<Vec<Edge>> {
        let owner_entity = source.entity.as_str().to_owned();
        let owner_id = uuid_of(source.id);
        let params: [&(dyn ToSql + Sync); 2] = [&owner_entity, &owner_id];
        let rows = self
            .tx
            .raw_query(SCAN, &params, None)
            .await
            .map_err(|e| EdgePgError::write(e.explanation).into_edge_sync())?;
        let mut edges = Vec::with_capacity(rows.len());
        for row in &rows {
            let decoded = decode_row(row).map_err(EdgePgError::into_edge_sync)?;
            edges.push(row_to_edge(decoded).map_err(EdgePgError::into_edge_sync)?);
        }
        edges.sort();
        Ok(edges)
    }
}

/// Reads one `spot_graph_edge` row.
pub(crate) fn decode_row(row: &tokio_postgres::Row) -> Result<EdgeRow, EdgePgError> {
    let bad = |e: tokio_postgres::Error| EdgePgError::decode(e.to_string());
    Ok(EdgeRow {
        edge_type: row.try_get(0).map_err(bad)?,
        owner_entity: row.try_get(1).map_err(bad)?,
        owner_id: row.try_get::<_, Uuid>(2).map_err(bad)?,
        from_entity: row.try_get(3).map_err(bad)?,
        from_id: row.try_get::<_, Uuid>(4).map_err(bad)?,
        to_entity: row.try_get(5).map_err(bad)?,
        to_id: row.try_get::<_, Uuid>(6).map_err(bad)?,
        properties: row.try_get::<_, serde_json::Value>(7).map_err(bad)?,
    })
}

impl EdgeWriter for PgEdgeWriter<'_> {
    fn add<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()> {
        Box::pin(async move { self.upsert(edge).await })
    }

    fn remove<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()> {
        Box::pin(async move { self.delete(edge).await })
    }

    fn update<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()> {
        Box::pin(async move { self.upsert(edge).await })
    }

    fn scan_by_source<'a>(&'a mut self, source: &'a NodeRef) -> EdgeFuture<'a, Vec<Edge>> {
        Box::pin(async move { self.edges_owned_by(source).await })
    }
}

/// The [`EdgeWriterFor`] factory for [`PgTransaction`]: what a host passes to
/// `EdgeParticipant::new` so edges commit and roll back with the document in PostgreSQL.
///
/// It holds nothing. Every writer it makes borrows the transaction the engine lends it, so an
/// edge is written through the very handle the document was written through (never a second
/// connection, which would survive a rollback).
///
/// The `site` argument does not pick a table: one database is one site (ADR-34), the tables carry
/// no site column, and the tenant boundary is the database the lent transaction belongs to. The
/// argument is deliberately not used to accept or refuse an invocation; refusing a write that
/// names another site is the participant's `for_site` check, and a host that serves several
/// tenants must give each its own engine and database.
#[derive(Debug, Clone, Copy, Default)]
pub struct PgEdgeWriters;

impl EdgeWriterFor<PgTransaction> for PgEdgeWriters {
    fn writer<'a>(
        &'a self,
        tx: &'a mut PgTransaction,
        _site: &'a SiteId,
    ) -> Box<dyn EdgeWriter + 'a> {
        Box::new(PgEdgeWriter::new(tx))
    }
}
