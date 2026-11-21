//! Incremental extraction from the change feed: upserts in their current state, deletions as
//! tombstones, and a watermark that is only handed back once the batch is fully delivered.
//!
//! A [`Watermark`] is a change-feed position plus the extraction epoch. `extract` reads the
//! change feed from that position, coalescing a document changed several times into its latest
//! state, and returns a batch of upserts (loaded through the same guarded plan as a full
//! export, so permissions and masks hold) and deletes. A delete is a **tombstone**, not an
//! absence. A watermark older than the feed retains, from another stream generation, or from a
//! different epoch is refused with `bulk.watermark_expired`, never a silent partial result.
//!
//! The new watermark is returned by [`IncrementalBatch::new_watermark`] only after the final
//! chunk, so a consumer that crashes re-reads its batch. Applying a batch twice is harmless:
//! upserts are keyed by document id and version, deletes are idempotent, and the batch carries
//! its `version`.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_storage_contract::{
    Change, ChangeFeed, ChangeOp, FeedPosition, RecordId, StorageResult, Value, Version,
};

use crate::error::{BulkError, BulkResult};
use crate::limits::BulkLimits;
use crate::page::{BoxFuture, PageReader};
use crate::plan::ExportPlan;

/// Reads pages of the change feed outside a transaction.
pub trait ChangeReader: Send + Sync {
    /// Returns up to `limit` changes after `after`.
    fn read_changes<'a>(
        &'a self,
        after: FeedPosition,
        limit: u32,
    ) -> BoxFuture<'a, StorageResult<Vec<Change>>>;
}

/// A [`ChangeReader`] over any change feed.
pub struct BackendChangeReader<B: ChangeFeed> {
    backend: B,
}

impl<B: ChangeFeed> BackendChangeReader<B> {
    /// Reads changes from `backend`.
    pub fn new(backend: B) -> Self {
        BackendChangeReader { backend }
    }
}

impl<B: ChangeFeed> ChangeReader for BackendChangeReader<B> {
    fn read_changes<'a>(
        &'a self,
        after: FeedPosition,
        limit: u32,
    ) -> BoxFuture<'a, StorageResult<Vec<Change>>> {
        Box::pin(async move { self.backend.read_changes(after, limit).await })
    }
}

/// A change-feed position plus the extraction epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watermark {
    /// The change-feed position.
    pub position: FeedPosition,
    /// The extraction epoch (a caller-owned counter; a full export starts a new one).
    pub epoch: u64,
}

/// A request for an incremental extract.
#[derive(Clone, Debug)]
pub struct IncrementalSpec {
    /// The DocType.
    pub doctype: String,
    /// The columns (must match the plan's columns; empty takes the plan's).
    pub columns: Vec<String>,
    /// Where extraction continues from.
    pub since: Watermark,
}

/// One delivered piece of an incremental batch.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BatchChunk {
    /// Rows changed since the watermark, in their current state (guarded and masked).
    pub upserts: Vec<Vec<Value>>,
    /// Ids deleted since the watermark.
    pub deletes: Vec<RecordId>,
}

/// A running incremental extract.
pub struct IncrementalBatch {
    plan: Arc<ExportPlan>,
    reader: Arc<dyn PageReader>,
    limits: BulkLimits,
    watermark: Watermark,
    version: u64,
    upsert_ids: Vec<RecordId>,
    delete_ids: Vec<RecordId>,
    upsert_at: usize,
    delete_at: usize,
    done: bool,
}

impl IncrementalBatch {
    /// Reads the change feed and builds a batch.
    ///
    /// # Errors
    ///
    /// `bulk.watermark_expired` when the watermark is too old, from another generation or a
    /// different epoch; `bulk.spec_invalid` when the columns do not match the plan;
    /// `bulk.storage` when the feed fails.
    pub async fn extract(
        plan: Arc<ExportPlan>,
        spec: &IncrementalSpec,
        reader: Arc<dyn PageReader>,
        changes: &dyn ChangeReader,
        epoch: u64,
        limits: BulkLimits,
    ) -> BulkResult<IncrementalBatch> {
        if spec.doctype != plan.doctype {
            return Err(BulkError::spec_invalid(format!(
                "the extract targets `{}` but the plan is `{}`",
                spec.doctype, plan.doctype
            )));
        }
        if spec.since.epoch != epoch {
            return Err(BulkError::watermark_expired(format!(
                "the watermark is from extraction epoch {} but the current epoch is {epoch}; run a full export and start again",
                spec.since.epoch
            )));
        }
        if !spec.columns.is_empty() {
            let plan_columns: Vec<String> = plan.columns.iter().map(|c| c.path.clone()).collect();
            if spec.columns != plan_columns {
                return Err(BulkError::spec_invalid(
                    "the extract's columns do not match the plan's",
                ));
            }
        }
        let limit = limits.rows_per_chunk.max(1);
        let mut after = spec.since.position;
        let mut latest = after;
        let mut coalesced: BTreeMap<RecordId, (ChangeOp, Version)> = BTreeMap::new();
        loop {
            let page = changes
                .read_changes(after, limit)
                .await
                .map_err(BulkError::from)?;
            if page.is_empty() {
                break;
            }
            let last = page.last().map(|c| c.position).unwrap_or(after);
            for change in &page {
                if change.document.entity.as_str() == spec.doctype {
                    coalesced.insert(change.document.id, (change.op, change.version));
                }
            }
            latest = last;
            if page.len() < limit as usize {
                break;
            }
            after = last;
        }
        let mut upsert_ids = Vec::new();
        let mut delete_ids = Vec::new();
        let mut version = 0u64;
        for (id, (op, v)) in &coalesced {
            version = version.max(v.0);
            match op {
                ChangeOp::Delete => delete_ids.push(*id),
                ChangeOp::Insert | ChangeOp::Update => upsert_ids.push(*id),
            }
        }
        Ok(IncrementalBatch {
            plan,
            reader,
            limits,
            watermark: Watermark {
                position: latest,
                epoch: spec.since.epoch,
            },
            version,
            upsert_ids,
            delete_ids,
            upsert_at: 0,
            delete_at: 0,
            done: false,
        })
    }

    /// The batch's version: the highest document version it carries (0 when empty). A consumer
    /// stores it and drops a batch it has already applied at the same version.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The new watermark, available only after [`IncrementalBatch::next_chunk`] returned `None`.
    #[must_use]
    pub fn new_watermark(&self) -> Option<Watermark> {
        self.done.then_some(self.watermark)
    }

    /// Delivers the next chunk, or `None` when the batch is complete.
    ///
    /// # Errors
    ///
    /// `bulk.storage` when the backend fails; `bulk.spec_invalid` when the plan cannot be
    /// queried.
    pub async fn next_chunk(&mut self) -> BulkResult<Option<BatchChunk>> {
        if self.done {
            return Ok(None);
        }
        let budget = self.limits.rows_per_chunk.max(1) as usize;
        let mut deletes = Vec::new();
        while deletes.len() < budget && self.delete_at < self.delete_ids.len() {
            deletes.push(self.delete_ids[self.delete_at]);
            self.delete_at += 1;
        }
        let take = budget.saturating_sub(deletes.len());
        let start = self.upsert_at;
        let end = (start + take).min(self.upsert_ids.len());
        let ids = self.upsert_ids[start..end].to_vec();
        self.upsert_at = end;
        let mut upserts = Vec::new();
        if !ids.is_empty() {
            let query = self.plan.parent_query_by_ids(&ids);
            let rows = self.reader.read(&query).await.map_err(BulkError::from)?;
            let mut found = std::collections::BTreeSet::new();
            for row in &rows.rows {
                let id_at = self
                    .plan
                    .parent_index("id")
                    .ok_or_else(|| BulkError::spec_invalid("id is not selected"))?;
                if let Some(Value::Id(id)) = row.get(id_at) {
                    found.insert(*id);
                    upserts.push(self.plan.parent_export_values(row));
                }
            }
            // A document the feed says changed but that is now gone (or no longer readable) is
            // a tombstone, not an upsert.
            for id in &ids {
                if !found.contains(id) {
                    deletes.push(*id);
                }
            }
        }
        if upserts.is_empty() && deletes.is_empty() {
            self.done = true;
            return Ok(None);
        }
        Ok(Some(BatchChunk { upserts, deletes }))
    }
}

impl std::fmt::Debug for IncrementalBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncrementalBatch")
            .field("doctype", &self.plan.doctype)
            .field("version", &self.version)
            .field("upserts", &self.upsert_ids.len())
            .field("deletes", &self.delete_ids.len())
            .field("done", &self.done)
            .finish()
    }
}
