//! The database full-text search projection (ADR-087 section 7; WP-STO-13.T1).
//!
//! ADR-087 decided that **database full-text is the platform default and the only guaranteed
//! search path**: an external engine is an optional read projection behind the same seam, built
//! only when a spike shows the database path misses a measured budget (section 7.1). This crate
//! is the database path: the `spot_search_doc` table, the maintenance that keeps it in step, the
//! read that filters by what the caller may read, the facet counts, the lag, the conformance
//! checks and a rebuild.
//!
//! # The pieces
//!
//! - [`ProjectionPlan`] — a DocType's `[search]` declaration compiled against the entity and its
//!   protection groups: which fields are searchable, at which weight, in which language, which
//!   fields are facets, whether typo tolerance and accent folding are on. It owns the SQL that
//!   builds a projection row, so a write and a rebuild cannot disagree.
//! - [`prepare`] — creates the table and the GIN index and checks the extensions the plans need.
//! - [`upsert`] / [`remove`] — maintenance, **in the same transaction as the document write**.
//! - [`search`] — one term, the caller's [`ReadScope`], facet counts and the projection's lag.
//! - [`rebuild`] — re-projects one DocType from the entity's own data, without writing it.
//! - [`check`] — the conformance checks that fail loudly.
//!
//! # Why maintenance is in the write's transaction, and not the outbox
//!
//! ADR-087 section 7.3 gives the reason the projection is a table and not a generated column:
//! a committed write is visible to the next search, with no separate pipeline. The ADR mentions
//! the outbox in exactly one place — the *external engine*, fed from the outbox, whose lag is
//! the `projection_lag` of section 7.8 — and that engine is not built. So this crate takes the
//! same-transaction path and says so: the document engine calls [`upsert`] (or [`remove`]) on
//! the transaction that wrote the document, and the projection therefore cannot be visible
//! without its document, or hidden after it. Both statements read the entity row **in that
//! transaction**, so the projection is built from what is actually stored, never from a copy in
//! an event payload. When the external engine arrives it consumes the same rows; it does not
//! replace these, and [`ProjectionLag`] already carries the field and the refusal its lag needs.
//!
//! # What a caller can rely on (ADR-087 section 7.5)
//!
//! | Guarantee | Where it is kept |
//! | --- | --- |
//! | A search returns only rows the caller may read | [`RowScope`] is part of the compiled filter |
//! | A field the caller may not read cannot affect a match or a rank | protected fields never enter [`ProjectionPlan::terms_sql`] |
//! | One operation shape and one error vocabulary | `Expr::Search`, `SearchError` with the contract's kinds |
//! | The projection is rebuildable, and its staleness is reported | [`rebuild`], [`ProjectionLag`] |
//! | Facet counts obey the same filter | the facet statement is the page's match set as a CTE |
//!
//! # The known warts, stated so they are not discovered in production
//!
//! - **`gin_fuzzy_search_limit` must be 0.** Above 0, GIN returns a *random subset* of the
//!   matches, which breaks stable paging. [`check`] fails loudly when it is not 0 (section 10).
//! - **GIN stores lexemes without their weights**, so a weighted match is rechecked against the
//!   table row; that is what `terms @@ tsquery` does, and it is why `terms` is `NOT NULL`.
//! - **GIN `fastupdate` keeps a pending list**, cleaned when it passes `gin_pending_list_limit`;
//!   an occasional search is a little slower until the cleanup runs. Nothing here turns it off.
//! - **`to_tsvector(NULL)` is `NULL`**, so every field in the vector is coalesced (a document
//!   with no text gets the empty vector, not a null row).
//! - **A rank is never a value.** `ts_rank` returns `real` and `Value` has no float (ADR-053), so
//!   the rank is an `ORDER BY` key inside the compiled SQL and nothing else (section 7.3).
//! - **`unaccent` is used only inside the projection.** Equality, ordering and unique keys keep
//!   treating `café` and `cafe` as different text (SEM-TXT-03).
//!
//! # Extensions (section 7.8) and migration (section 7.9)
//!
//! `unaccent` is required by a projection that folds accents and `pg_trgm` by one that declares
//! typo tolerance; a missing extension refuses [`prepare`] with the extension and the remedy
//! named, and never degrades to a different behaviour. `spot_search_doc` is a platform table with
//! the `spot_` prefix the migrator's gate requires, and the GIN index is created concurrently for
//! an existing tenant ([`concurrent_index_sql`] is the statement the migrator plans as
//! `Run::Outside`; [`index_is_valid`] is the check that it became valid).
//!
//! # Not in this crate
//!
//! An external search engine, search over protected fields (a future option, written down in
//! section 7.7 and not briefed), snippets (`ts_headline`, which must run over the already
//! permission-filtered page), the Admin Center UI, and the `spot-meta` compilation of the
//! `[search]` declaration into a catalog — this crate consumes the [`SearchDef`] it produces.
//!
//! [`SearchDef`]: spot_storage_contract::SearchDef

mod conformance;
mod error;
mod lag;
mod maintain;
mod plan;
mod protect;
mod rebuild;
mod schema;
mod search;

#[cfg(test)]
mod fixture;

pub use conformance::{ConformanceReport, check, read as read_conformance};
pub use error::{FIELD_DENIED, SearchError};
pub use lag::{
    DEFAULT_BOUND_MS, METRIC as PROJECTION_LAG_METRIC, ProjectionLag, measure, measure_doctype,
};
pub use maintain::{remove, upsert};
pub use plan::{Language, PlannedField, ProjectionPlan, configuration_for};
pub use protect::Protection;
pub use rebuild::{RebuildReport, planned_doctypes, rebuild, rebuild_all};
pub use schema::{
    DOC_ID, DOCTYPE, DOCTYPE_INDEX, FACETS, TABLE, TENANT_LANGUAGE, TERMS, TERMS_INDEX, UPDATED_AT,
    check_extensions, concurrent_index_sql, create_doctype_index_sql, create_index_sql,
    create_table_sql, index_is_valid, installed_extensions, prepare,
};
pub use search::{
    FacetBucket, FacetQuery, ReadScope, RowScope, SearchHit, SearchPage, SearchRequest,
    compile_facets, compile_page, search,
};
