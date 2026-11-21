//! Row paths from the positions after a patch.
//!
//! A patch replaces a table, so the row numbers an error should name are the ones in the **new**
//! table (the tests of the L3 suite: "row errors name rows by their new position"). These helpers
//! turn a table, as it is after the patch, into `items[3].qty` paths, rows counted from 1.

use std::collections::BTreeMap;

use spot_app_api::ChildRow;
use spot_app_api::error::FieldPath;
use spot_app_api::prelude::RecordId;

/// A row with its number in the table (from 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowNumber {
    /// The row number, 1 for the first row.
    pub number: u32,
    /// The row's id.
    pub id: RecordId,
}

/// `items[3].qty`: the field `field` of row number `row_number` (from 1) of `table`.
pub fn row_path(table: &str, row_number: u32, field: &str) -> FieldPath {
    FieldPath::row(table, row_number, field)
}

/// The rows of a table numbered from 1, in order.
pub fn renumber(rows: &[ChildRow]) -> Vec<RowNumber> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| RowNumber {
            number: u32::try_from(i + 1).unwrap_or(u32::MAX),
            id: r.id,
        })
        .collect()
}

/// The row number (from 1) of the row with `id` in `rows`: its first occurrence; `None` if it is
/// not there.
pub fn row_number_of(rows: &[ChildRow], id: RecordId) -> Option<u32> {
    rows.iter()
        .position(|r| r.id == id)
        .map(|i| u32::try_from(i + 1).unwrap_or(u32::MAX))
}

/// The path of `field` in the row with `id`, by that row's position in `rows` (the table after the
/// patch); `None` if no such row.
pub fn path_of(table: &str, rows: &[ChildRow], id: RecordId, field: &str) -> Option<FieldPath> {
    row_number_of(rows, id).map(|n| row_path(table, n, field))
}

/// The path of `field` for every row, by row id (the first occurrence of an id wins).
pub fn row_paths(table: &str, rows: &[ChildRow], field: &str) -> BTreeMap<RecordId, FieldPath> {
    let mut out = BTreeMap::new();
    for r in renumber(rows) {
        out.entry(r.id)
            .or_insert_with(|| row_path(table, r.number, field));
    }
    out
}
