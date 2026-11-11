//! The `edge_repair` operation: apply a drift report's changes in batches, with an audit entry
//! per batch.
//!
//! A repair reads a report and turns its entries into [`EdgeChanges`]; it never computes drift
//! itself, so it can only add an edge the report named as missing, update one it named as
//! mismatched and remove one it named as extra. **No edge without a matching report entry is
//! ever touched.** The changes are applied in batches of a caller-set size, each batch in its
//! own transaction with one `spot_edge_repair_audit` row, so a repair is resumable and its progress is
//! recorded.

use std::sync::Arc;

use spot_edge_sync::{EdgeChanges, apply_in};
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{Catalog, Isolation, Name, StorageBackend, Transaction, TxOptions};
use time::OffsetDateTime;
use tokio_postgres::types::ToSql;

use crate::error::{EdgePgError, Result};
use crate::report::read_report;
use crate::writer::PgEdgeWriter;

/// How many changes one repair batch holds by default.
pub const DEFAULT_BATCH_SIZE: usize = 200;

/// One applied batch, as audited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchAudit {
    /// The batch number, from zero.
    pub batch_no: u32,
    /// Edges added.
    pub added: u32,
    /// Edges removed.
    pub removed: u32,
    /// Edges whose properties were updated.
    pub updated: u32,
}

/// What one repair did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairReport {
    /// The report that was repaired.
    pub report_key: String,
    /// The batches applied, in order.
    pub batches: Vec<BatchAudit>,
    /// Edges added in total.
    pub added: u64,
    /// Edges removed in total.
    pub removed: u64,
    /// Edges updated in total.
    pub updated: u64,
}

impl RepairReport {
    /// Whether the repair changed nothing.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.batches.is_empty()
    }
}

/// The repair operation over one tenant.
pub struct EdgeRepair {
    backend: PgBackend,
    catalog: Arc<Catalog>,
    batch_size: usize,
}

impl EdgeRepair {
    /// A repair over `backend` and `catalog`.
    #[must_use]
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self {
            backend,
            catalog,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    /// The same repair with a different batch size (at least one).
    #[must_use]
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    /// Applies the report `report_key` in batches, recording one audit row per batch.
    ///
    /// `now` is the instant the repair happened; the operation reads no clock.
    ///
    /// # Errors
    /// `edge_pg.report_not_found` when the report is missing, `edge_pg.unavailable` when the
    /// store cannot be reached, `edge_pg.write_failed` when a batch cannot be applied.
    pub async fn repair(&self, now: OffsetDateTime, report_key: &str) -> Result<RepairReport> {
        let mut read = self.begin("edge_repair_read").await?;
        let report = read_report(&mut read, report_key).await?;
        let changes = report.changes()?;
        read.rollback()
            .await
            .map_err(|e| EdgePgError::write(e.explanation))?;

        let mut out = RepairReport {
            report_key: report_key.to_owned(),
            batches: Vec::new(),
            added: 0,
            removed: 0,
            updated: 0,
        };
        for (index, batch) in split(&changes, self.batch_size).into_iter().enumerate() {
            let batch_no =
                u32::try_from(index).map_err(|_| EdgePgError::write("too many repair batches"))?;
            let added = u32::try_from(batch.added.len()).unwrap_or(u32::MAX);
            let removed = u32::try_from(batch.removed.len()).unwrap_or(u32::MAX);
            let updated = u32::try_from(batch.updated.len()).unwrap_or(u32::MAX);
            let mut tx = self.begin("edge_repair").await?;
            {
                let mut writer = PgEdgeWriter::new(&mut tx);
                apply_in(&mut writer, &batch)
                    .await
                    .map_err(|e| EdgePgError::write(e.to_string()))?;
            }
            audit(&mut tx, report_key, batch_no, now, added, removed, updated).await?;
            tx.commit()
                .await
                .map_err(|e| EdgePgError::write(e.explanation))?;
            out.batches.push(BatchAudit {
                batch_no,
                added,
                removed,
                updated,
            });
            out.added += u64::from(added);
            out.removed += u64::from(removed);
            out.updated += u64::from(updated);
        }
        Ok(out)
    }

    async fn begin(&self, label: &str) -> Result<PgTransaction> {
        let name = Name::new(label)
            .map_err(|_| EdgePgError::write("the transaction label is not a valid name"))?;
        let options = TxOptions::new(name, Isolation::ReadCommitted);
        self.backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|e| EdgePgError::Unavailable {
                message: e.explanation,
            })
    }
}

/// One audit row for a batch.
async fn audit(
    tx: &mut PgTransaction,
    report_key: &str,
    batch_no: u32,
    now: OffsetDateTime,
    added: u32,
    removed: u32,
    updated: u32,
) -> Result<()> {
    let batch_no = i32::try_from(batch_no).unwrap_or(i32::MAX);
    let added = i32::try_from(added).unwrap_or(i32::MAX);
    let removed = i32::try_from(removed).unwrap_or(i32::MAX);
    let updated = i32::try_from(updated).unwrap_or(i32::MAX);
    let params: [&(dyn ToSql + Sync); 6] =
        [&report_key, &batch_no, &now, &added, &removed, &updated];
    tx.raw_execute(
        "INSERT INTO spot_edge_repair_audit \
         (report_key, batch_no, repaired_at, added, removed, updated) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (report_key, batch_no) DO UPDATE SET repaired_at = EXCLUDED.repaired_at, \
         added = EXCLUDED.added, removed = EXCLUDED.removed, updated = EXCLUDED.updated",
        &params,
    )
    .await
    .map_err(|e| EdgePgError::write(e.explanation))?;
    Ok(())
}

/// Splits a change into batches of at most `size` individual changes, in apply order (removes,
/// then updates, then adds).
fn split(changes: &EdgeChanges, size: usize) -> Vec<EdgeChanges> {
    let mut out: Vec<EdgeChanges> = Vec::new();
    let mut current = EdgeChanges::default();
    let mut count = 0usize;
    for edge in &changes.removed {
        maybe_flush(&mut current, &mut count, &mut out, size);
        current.removed.insert(edge.clone());
        count += 1;
    }
    for update in &changes.updated {
        maybe_flush(&mut current, &mut count, &mut out, size);
        current.updated.push(update.clone());
        count += 1;
    }
    for edge in &changes.added {
        maybe_flush(&mut current, &mut count, &mut out, size);
        current.added.insert(edge.clone());
        count += 1;
    }
    if count > 0 {
        out.push(current);
    }
    out
}

/// Emits the batch being built when it has reached `size`.
fn maybe_flush(
    current: &mut EdgeChanges,
    count: &mut usize,
    out: &mut Vec<EdgeChanges>,
    size: usize,
) {
    if *count >= size {
        out.push(std::mem::take(current));
        *count = 0;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_edge_sync::Edge;
    use spot_graph_model::EdgeTypeId;
    use spot_storage_contract::{Name, NodeRef, RecordId};

    fn edge(index: u8) -> Edge {
        let mut id = [0u8; 16];
        // A UUIDv7: the version nibble is 7, the rest fixed by `index`.
        id[6] = 0x70;
        id[15] = index;
        let from = NodeRef {
            entity: Name::new("invoice").unwrap(),
            id: RecordId::from_bytes(id).unwrap(),
        };
        let to = NodeRef {
            entity: Name::new("party").unwrap(),
            id: RecordId::new_v7(),
        };
        Edge::new(EdgeTypeId::of("invoice.party"), from, to)
    }

    #[test]
    fn split_batches_every_change_and_keeps_apply_order() {
        let mut changes = EdgeChanges::default();
        changes.removed.insert(edge(1));
        changes.removed.insert(edge(2));
        let three = edge(3);
        changes.updated.push(spot_edge_sync::EdgeUpdate {
            key: three.key(),
            before: three.clone(),
            after: three,
        });
        changes.added.insert(edge(4));
        changes.added.insert(edge(5));

        let batches = split(&changes, 2);
        assert_eq!(batches.len(), 3);
        let total: usize = batches.iter().map(EdgeChanges::len).sum();
        assert_eq!(total, changes.len());
        // The first batch holds the two removes; every change is present exactly once, and the
        // order within every batch is removes, then updates, then adds.
        assert_eq!(batches[0].removed.len(), 2);
        assert!(batches[0].updated.is_empty() && batches[0].added.is_empty());
        let removed: usize = batches.iter().map(|b| b.removed.len()).sum();
        let updated: usize = batches.iter().map(|b| b.updated.len()).sum();
        let added: usize = batches.iter().map(|b| b.added.len()).sum();
        assert_eq!((removed, updated, added), (2, 1, 2));
    }

    #[test]
    fn an_empty_change_splits_into_no_batches() {
        assert!(split(&EdgeChanges::default(), 10).is_empty());
    }
}
