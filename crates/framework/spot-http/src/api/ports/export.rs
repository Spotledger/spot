//! The bulk data seam of `/api/v1/export*` (`export_start`, `export_next`, `export_extract`,
//! contract 0.4.0, WP-INT-03.T2): permission-filtered, field-guarded export of one DocType in
//! bounded chunks, and incremental extraction with tombstones.
//!
//! `spot-http` does not depend on the bulk crate. It defines [`BulkPort`], the three operations
//! in the shapes the router needs, and the host writes the adapter over `spot-bulk-data`.
//! Everything that decides is the port's: who the caller is (from `ctx.credential` on
//! `ctx.site`), whether the caller holds the `export` right, which rows and columns the caller
//! may take, whether a cursor or watermark is the caller's own. The routes pass **no user and
//! no tenant**: there is no way to ask for another caller's export through this trait.
//!
//! Cursors and watermarks cross as the contract's opaque text. The routes check their shape
//! (length and URL-safe characters) and never read the inside.
//!
//! A chunk crosses as one or more named parts; the route decides the framing (a plain body, or
//! `multipart/mixed` when the export includes child tables) and the headers.

use serde_json::Value as Json;

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`BulkPort`].
pub const KEY: PortKey = PortKey::of::<dyn BulkPort>();

/// What a port call answers.
pub type BulkResult<T> = Result<T, ApiError>;

/// The longest cursor or watermark the contract allows (`maxLength: 2048`).
pub const MAX_TOKEN: usize = 2048;

/// The export format (the contract's `format`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormatIn {
    /// `text/csv; charset=utf-8`.
    Csv,
    /// `application/x-ndjson`.
    Ndjson,
}

impl ExportFormatIn {
    /// The contract's text.
    pub fn as_str(self) -> &'static str {
        match self {
            ExportFormatIn::Csv => "csv",
            ExportFormatIn::Ndjson => "ndjson",
        }
    }

    /// The response content type of a part in this format.
    pub fn content_type(self) -> &'static str {
        match self {
            ExportFormatIn::Csv => "text/csv; charset=utf-8",
            ExportFormatIn::Ndjson => "application/x-ndjson",
        }
    }
}

/// One order key (the contract's `OrderKey`), shape-checked by the route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderKeyIn {
    /// The field's storage name.
    pub field: String,
    /// `true` for `dir: desc`.
    pub descending: bool,
    /// `Some(true)` for `nulls: first`, `Some(false)` for `nulls: last`, `None` for the default.
    pub nulls_first: Option<bool>,
}

/// The body of `export_start` (the contract's `ExportSpec`), shape-checked by the route.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportStartIn {
    /// The DocType's storage name.
    pub doctype: String,
    /// The requested columns; empty means every column the caller may export.
    pub columns: Vec<String>,
    /// The contract's `Filter`, as sent; the port parses and validates it against the metadata.
    pub filter: Option<Json>,
    /// The sort keys (at most 8); empty takes the default.
    pub order: Vec<OrderKeyIn>,
    /// The format.
    pub format: ExportFormatIn,
    /// Child tables as further parts.
    pub include_children: bool,
    /// Stop after this many parent rows.
    pub max_rows: Option<u64>,
}

/// The body of `export_extract` (the contract's `ExtractRequest`), shape-checked by the route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractIn {
    /// The DocType's storage name.
    pub doctype: String,
    /// The requested columns; empty means every column the caller may export.
    pub columns: Vec<String>,
    /// The opaque watermark.
    pub watermark: String,
    /// The most rows in the page, 1 to 5,000.
    pub limit: u32,
}

/// One part of a chunk: a table's rows in the export's format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportPartOut {
    /// The table the part holds: the DocType for the parent, the table field for a child.
    pub name: String,
    /// The bytes (a CSV header only on the part's first chunk).
    pub bytes: Vec<u8>,
}

/// One chunk of an export, with the headers that go with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportChunkOut {
    /// The format of every part.
    pub format: ExportFormatIn,
    /// `true` when the export includes child tables: the response is `multipart/mixed` and each
    /// part carries its `name`.
    pub multipart: bool,
    /// The parts, in order (the parent first).
    pub parts: Vec<ExportPartOut>,
    /// The opaque cursor of the next chunk; `None` when the export is complete.
    pub cursor: Option<String>,
    /// Parent rows in this chunk.
    pub rows: u64,
    /// The incremental-extraction watermark taken when the export began.
    pub watermark: String,
}

/// One page of changes (the contract's `ExtractPage`).
#[derive(Clone, Debug, PartialEq)]
pub struct ExtractPageOut {
    /// The current state of each document that changed, as the contract's `ExtractRow`
    /// (`id`, `version` and the exported columns, in the value encoding).
    pub upserts: Vec<Json>,
    /// The ids of the documents deleted (or no longer visible to the caller): tombstones.
    pub deletes: Vec<String>,
    /// The opaque watermark after this page.
    pub new_watermark: String,
    /// Whether another call from `new_watermark` has more.
    pub has_more: bool,
}

/// Bulk export and incremental extraction for the caller the credential names.
pub trait BulkPort: Send + Sync {
    /// Starts an export and returns its first chunk.
    fn start<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        spec: ExportStartIn,
    ) -> BoxFuture<'a, BulkResult<ExportChunkOut>>;

    /// Continues an export from the cursor of the previous chunk.
    fn next<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        cursor: &'a str,
    ) -> BoxFuture<'a, BulkResult<ExportChunkOut>>;

    /// One page of what changed in a DocType since a watermark.
    fn extract<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: ExtractIn,
    ) -> BoxFuture<'a, BulkResult<ExtractPageOut>>;
}
