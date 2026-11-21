//! [`ExportStream`]: bounded, keyset-paged, resumable streaming of a checked plan.
//!
//! The stream is **pull-based**: each call to [`ExportStream::next_chunk`] reads at most one
//! page (one short read outside any transaction) and returns at most one bounded chunk. A slow
//! consumer holds no database resource between calls. Dropping the stream releases its
//! coordinator lease and records a cancel.

use std::collections::VecDeque;
use std::sync::Arc;

use spot_storage_contract::{Expr, Value};

use crate::cursor::{Cursor, CursorBody, CursorKey};
use crate::error::{BulkError, BulkResult};
use crate::format;
use crate::limits::{
    BulkCoordinator, BulkLimits, ExportAuditEvent, ExportAuditSink, ExportLease, ExportPhase,
    Progress, Telemetry,
};
use crate::page::PageReader;
use crate::plan::ExportPlan;
use crate::spec::Part;

/// One bounded piece of an export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Which part this chunk belongs to.
    pub part: Part,
    /// The bytes (including a CSV header only on the part's first chunk).
    pub bytes: Vec<u8>,
    /// How many data rows the chunk holds.
    pub rows: usize,
}

/// Everything an export stream needs from its host.
pub struct StreamContext {
    /// Where pages are read.
    pub reader: Arc<dyn PageReader>,
    /// The cursor signing key.
    pub key: CursorKey,
    /// The principal (for the cursor and the audit record).
    pub principal: String,
    /// The tenant (for the coordinator).
    pub tenant: String,
    /// The bounds.
    pub limits: BulkLimits,
    /// The concurrency and rate enforcement point.
    pub coordinator: Arc<dyn BulkCoordinator>,
    /// Where progress goes.
    pub telemetry: Arc<dyn Telemetry>,
    /// Where audit records go.
    pub audit: Arc<dyn ExportAuditSink>,
}

impl std::fmt::Debug for StreamContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamContext")
            .field("principal", &self.principal)
            .field("tenant", &self.tenant)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// A running export.
pub struct ExportStream {
    plan: Arc<ExportPlan>,
    reader: Arc<dyn PageReader>,
    key: CursorKey,
    principal: String,
    limits: BulkLimits,
    lease: Option<Box<dyn ExportLease>>,
    telemetry: Arc<dyn Telemetry>,
    audit: Arc<dyn ExportAuditSink>,
    parts: Vec<Part>,
    part_index: usize,
    position: Vec<Value>,
    page: VecDeque<(Vec<Value>, Vec<Value>)>,
    rows: u64,
    bytes: u64,
    header_emitted: bool,
    finished: bool,
    cancel_recorded: bool,
}

impl ExportStream {
    /// Starts an export from the beginning.
    ///
    /// # Errors
    ///
    /// `bulk.limit` when a concurrency bound is reached.
    pub fn start(plan: Arc<ExportPlan>, ctx: StreamContext) -> BulkResult<ExportStream> {
        ExportStream::open(plan, ctx, 0, Vec::new(), 0)
    }

    /// Resumes an export from a cursor, continuing exactly after the last delivered row.
    ///
    /// # Errors
    ///
    /// `bulk.cursor_invalid` when the cursor is forged, malformed, or belongs to another plan
    /// or principal; `bulk.limit` when a concurrency bound is reached.
    pub fn resume(
        plan: Arc<ExportPlan>,
        ctx: StreamContext,
        cursor: &Cursor,
    ) -> BulkResult<ExportStream> {
        let body = cursor.decode(&ctx.key)?;
        if body.plan_digest != plan.digest {
            return Err(BulkError::cursor_invalid(
                "the cursor belongs to another export plan",
            ));
        }
        if body.principal != ctx.principal {
            return Err(BulkError::cursor_invalid(
                "the cursor belongs to another principal",
            ));
        }
        let part_index = usize::try_from(body.part)
            .map_err(|_| BulkError::cursor_invalid("the cursor part is out of range"))?;
        ExportStream::open(plan, ctx, part_index, body.keys, body.rows)
    }

    fn open(
        plan: Arc<ExportPlan>,
        ctx: StreamContext,
        part_index: usize,
        position: Vec<Value>,
        rows: u64,
    ) -> BulkResult<ExportStream> {
        let parts = plan.parts();
        if part_index >= parts.len() {
            return Err(BulkError::cursor_invalid(
                "the cursor names a part the plan does not have",
            ));
        }
        let lease = ctx
            .coordinator
            .acquire(&ctx.tenant, &ctx.principal, &ctx.limits)?;
        let header_emitted = !position.is_empty();
        let stream = ExportStream {
            plan,
            reader: ctx.reader,
            key: ctx.key,
            principal: ctx.principal,
            limits: ctx.limits,
            lease: Some(lease),
            telemetry: ctx.telemetry,
            audit: ctx.audit,
            parts,
            part_index,
            position,
            page: VecDeque::new(),
            rows,
            bytes: 0,
            header_emitted,
            finished: false,
            cancel_recorded: false,
        };
        stream.record(ExportPhase::Start);
        Ok(stream)
    }

    /// The plan this stream runs.
    #[must_use]
    pub fn plan(&self) -> &ExportPlan {
        &self.plan
    }

    /// Rows delivered so far.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Bytes delivered so far.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Whether the export is finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// The cursor for the current position.
    ///
    /// # Errors
    ///
    /// `bulk.cursor_invalid` when the key is unusable.
    pub fn cursor(&self) -> BulkResult<Cursor> {
        Cursor::encode(
            &self.key,
            &CursorBody {
                plan_digest: self.plan.digest,
                principal: self.principal.clone(),
                part: u32::try_from(self.part_index).unwrap_or(u32::MAX),
                rows: self.rows,
                keys: self.position.clone(),
            },
        )
    }

    /// Delivers the next bounded chunk, or `None` when the export is complete.
    ///
    /// # Errors
    ///
    /// `bulk.limit` when the row cap or the rate budget is reached; `bulk.storage` when the
    /// backend fails; `bulk.cursor_invalid` when a position cannot be continued.
    pub async fn next_chunk(&mut self) -> BulkResult<Option<Chunk>> {
        if self.finished {
            return Ok(None);
        }
        if self.plan.read_none {
            self.finish();
            return Ok(None);
        }
        loop {
            if self.page.is_empty() {
                self.fetch().await?;
                if self.page.is_empty() {
                    if !self.advance_part() {
                        self.finish();
                        return Ok(None);
                    }
                    continue;
                }
            }
            return Ok(Some(self.emit_chunk()?));
        }
    }

    /// Fetches one page for the current part.
    async fn fetch(&mut self) -> BulkResult<()> {
        let part = self
            .parts
            .get(self.part_index)
            .cloned()
            .ok_or_else(|| BulkError::cursor_invalid("no such part"))?;
        let limit = self.page_limit();
        let keyset = self.keyset_for(&part)?;
        let query = match &part {
            Part::Parent => self.plan.parent_query(keyset, limit),
            Part::Child(table) => self
                .plan
                .child_query(table, keyset, limit)
                .ok_or_else(|| BulkError::spec_invalid("the child part is not in the plan"))?,
        };
        let rows = self.reader.read(&query).await?;
        self.page.clear();
        for row in rows.rows {
            let keys = self.keys_for(&part, &row)?;
            self.page.push_back((row, keys));
        }
        Ok(())
    }

    /// The keyset predicate for the current position, or `None` at the start of a part.
    fn keyset_for(&self, part: &Part) -> BulkResult<Option<Expr>> {
        if self.position.is_empty() {
            return Ok(None);
        }
        match part {
            Part::Parent => Ok(self.plan.parent_keyset(&self.position)),
            Part::Child(_) => {
                let parent = match self.position.first() {
                    Some(Value::Id(id)) => *id,
                    _ => {
                        return Err(BulkError::cursor_invalid(
                            "the cursor has no child parent id",
                        ));
                    }
                };
                let idx = match self.position.get(1) {
                    Some(Value::Int(i)) => *i,
                    _ => {
                        return Err(BulkError::cursor_invalid(
                            "the cursor has no child position",
                        ));
                    }
                };
                Ok(Some(self.plan.child_keyset(parent, idx)))
            }
        }
    }

    /// The keyset keys of one fetched row.
    fn keys_for(&self, part: &Part, row: &[Value]) -> BulkResult<Vec<Value>> {
        match part {
            Part::Parent => {
                let mut keys = Vec::with_capacity(self.plan.order.len() + 1);
                for key in &self.plan.order {
                    let at = self
                        .plan
                        .parent_index(&key.field)
                        .ok_or_else(|| BulkError::spec_invalid("the order key is not selected"))?;
                    keys.push(row.get(at).cloned().unwrap_or(Value::Null));
                }
                let id_at = self
                    .plan
                    .parent_index("id")
                    .ok_or_else(|| BulkError::spec_invalid("id is not selected"))?;
                keys.push(row.get(id_at).cloned().unwrap_or(Value::Null));
                Ok(keys)
            }
            Part::Child(table) => {
                let parent = self
                    .plan
                    .child_index(table, crate::spec::PARENT_COLUMN)
                    .ok_or_else(|| BulkError::spec_invalid("parent is not selected"))?;
                let idx = self
                    .plan
                    .child_index(table, crate::spec::IDX_COLUMN)
                    .ok_or_else(|| BulkError::spec_invalid("idx is not selected"))?;
                Ok(vec![
                    row.get(parent).cloned().unwrap_or(Value::Null),
                    row.get(idx).cloned().unwrap_or(Value::Null),
                ])
            }
        }
    }

    /// Emits a chunk from the buffered page, advancing the position to the last emitted row.
    fn emit_chunk(&mut self) -> BulkResult<Chunk> {
        let part = self
            .parts
            .get(self.part_index)
            .cloned()
            .ok_or_else(|| BulkError::cursor_invalid("no such part"))?;
        let names = self.plan.column_names(&part);
        let mut bytes = Vec::new();
        if !self.header_emitted {
            bytes.extend_from_slice(&format::header(&names, self.plan.format));
            self.header_emitted = true;
        }
        let mut count = 0usize;
        while let Some((row, keys)) = self.page.front() {
            let values = self.export_values(&part, row);
            let row_bytes = format::row(&names, &values, self.plan.format, self.plan.hardening);
            if count > 0 && bytes.len() + row_bytes.len() > self.limits.bytes_per_chunk {
                break;
            }
            bytes.extend_from_slice(&row_bytes);
            self.position = keys.clone();
            count += 1;
            self.page.pop_front();
        }
        self.rows += u64::try_from(count).unwrap_or(u64::MAX);
        if let Some(cap) = self.plan.max_rows
            && self.rows > cap
        {
            return Err(BulkError::limit(format!(
                "the export reached its cap of {cap} rows"
            )));
        }
        self.bytes += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if let Some(lease) = &self.lease {
            lease.take_rows(u64::try_from(count).unwrap_or(u64::MAX))?;
        }
        self.telemetry.progress(&Progress {
            rows: self.rows,
            bytes: self.bytes,
        });
        Ok(Chunk {
            part,
            bytes,
            rows: count,
        })
    }

    /// The export values of one row (the planned columns, masked where planned).
    fn export_values(&self, part: &Part, row: &[Value]) -> Vec<Value> {
        match part {
            Part::Parent => {
                let mut out = Vec::with_capacity(self.plan.columns.len());
                for (i, column) in self.plan.columns.iter().enumerate() {
                    let value = row.get(i).cloned().unwrap_or(Value::Null);
                    out.push(mask_value(value, column.mask.as_ref()));
                }
                out
            }
            Part::Child(table) => {
                let columns = self
                    .plan
                    .children
                    .iter()
                    .find(|c| &c.table == table)
                    .map(|c| c.columns.clone())
                    .unwrap_or_default();
                let mut out = Vec::with_capacity(columns.len());
                for (i, column) in columns.iter().enumerate() {
                    let value = row.get(i).cloned().unwrap_or(Value::Null);
                    out.push(mask_value(value, column.mask.as_ref()));
                }
                out
            }
        }
    }

    /// Moves to the next part; `false` when there is none.
    fn advance_part(&mut self) -> bool {
        self.part_index += 1;
        if self.part_index >= self.parts.len() {
            return false;
        }
        self.position.clear();
        self.header_emitted = false;
        self.page.clear();
        true
    }

    /// The page query limit: the chunk bound, tightened by the remaining row cap (plus one, so
    /// an over-cap page is detected rather than silently truncated).
    fn page_limit(&self) -> u32 {
        let base = self.limits.rows_per_chunk;
        match self.plan.max_rows {
            Some(cap) => {
                let remaining = cap.saturating_sub(self.rows).saturating_add(1);
                u32::try_from(remaining.min(u64::from(base))).unwrap_or(u32::MAX)
            }
            None => base,
        }
    }

    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.record(ExportPhase::Finish);
        }
    }

    fn record(&self, phase: ExportPhase) {
        self.audit.record(&ExportAuditEvent {
            who: self.principal.clone(),
            doctype: self.plan.doctype.clone(),
            columns: self.plan.columns.iter().map(|c| c.path.clone()).collect(),
            filter_digest: self.plan.filter_digest.clone(),
            plan_digest: hex(&self.plan.digest),
            row_count: self.rows,
            phase,
        });
    }
}

impl Drop for ExportStream {
    fn drop(&mut self) {
        if !self.finished && !self.cancel_recorded {
            self.cancel_recorded = true;
            self.record(ExportPhase::Cancel);
        }
    }
}

impl std::fmt::Debug for ExportStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportStream")
            .field("doctype", &self.plan.doctype)
            .field("part", &self.part_index)
            .field("rows", &self.rows)
            .field("finished", &self.finished)
            .finish()
    }
}

/// Applies a mask to a value: text is masked; anything else becomes null (the same choice the
/// document read path makes).
fn mask_value(value: Value, mask: Option<&spot_permissions::MaskFormat>) -> Value {
    match mask {
        None => value,
        Some(format) => match value {
            Value::Text(text) => Value::Text(spot_permissions::mask(&text, format)),
            _ => Value::Null,
        },
    }
}

/// A lowercase hex rendering of a digest.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
