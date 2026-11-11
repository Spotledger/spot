#![forbid(unsafe_code)]
//! Child-table operations as pure functions (WP-DOC-03.T1; ADR-34, ADR-55, ADR-56).
//!
//! The lifecycle engine (WP-DOC-04), the event builder and the L3 child-table suite share these
//! functions, so what counts as "a row moved" or "a row changed" is decided once:
//!
//! - [`diff_rows`] and [`table_changes`]: what happened to a table's rows between two versions of
//!   a document, matched by row id only.
//! - [`row_path`], [`renumber`], [`path_of`]: `items[3].qty` paths, rows counted **from 1**, from
//!   the positions *after* a patch, for error reporting.
//! - [`clone_rows`], [`clone_tables`]: rows for amend and duplicate, with fresh ids.
//! - [`validate_table_shape`]: what [`spot_document`'s `validate::check`](
//!   https://docs.rs/spot-document) does not check: row id uniqueness across the whole document,
//!   values that belong to another table, repeated multiselect targets.
//!
//! Nothing here reads a clock, a database or the registry; every function is a pure function of
//! its arguments.
//!
//! # Row numbers
//!
//! Every position in this crate is a **row number: 1 for the first row**, the same number a user
//! sees and the same number `FieldPath` carries (`items[1].qty`). Storage's `idx` column counts
//! from 0; the mapping layer converts.

mod clone;
mod diff;
mod paths;
mod shape;

pub use clone::{clone_rows, clone_rows_checked, clone_tables};
pub use diff::{Moved, RowChange, TableDiff, diff_rows, table_changes};
pub use paths::{RowNumber, path_of, renumber, row_number_of, row_path, row_paths};
pub use shape::{INVALID_ROW, INVALID_VALUE, validate_table_shape};
