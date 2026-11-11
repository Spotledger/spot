//! Cloning rows for amend and duplicate: same values, same order, fresh ids.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::error::FieldPath;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{AppError, AppResult, ChildRow};

use crate::shape::INVALID_ROW;

/// Clones `rows`: the same values in the same order, each row under a fresh id from `new_ids`.
///
/// **Nothing keeps pointing at an old row.** A row id is not copied, and a value that holds the old
/// id of one of the cloned rows (a link from one row to another, `Value::Id`) is replaced by that
/// row's new id, so the clone is self-contained. Links to anything else (a customer, an item)
/// are copied as they are.
///
/// `new_ids` is called exactly once per row, in row order. It must hand out ids that are unused;
/// [`clone_rows_checked`] verifies that.
pub fn clone_rows(rows: &[ChildRow], new_ids: &mut dyn FnMut() -> RecordId) -> Vec<ChildRow> {
    let renamed: BTreeMap<RecordId, RecordId> = rows.iter().map(|r| (r.id, new_ids())).collect();
    clone_with(rows, &renamed)
}

/// [`clone_rows`] that fails with `document.invalid_row` (at the row's `id`) if a generated id is
/// already an id of one of the cloned rows, or is handed out twice. `table` names the table in
/// the error path.
pub fn clone_rows_checked(
    table: &str,
    rows: &[ChildRow],
    new_ids: &mut dyn FnMut() -> RecordId,
) -> AppResult<Vec<ChildRow>> {
    let cloned = clone_rows(rows, new_ids);
    check_fresh(table, rows, &cloned)?;
    Ok(cloned)
}

/// Clones every table of a document with one id mapping, so a link from a row of one table to a
/// row of another follows the clone too. Tables are processed in name order and rows in order, so
/// the generator is called in a fixed sequence.
pub fn clone_tables(
    tables: &BTreeMap<Name, Vec<ChildRow>>,
    new_ids: &mut dyn FnMut() -> RecordId,
) -> BTreeMap<Name, Vec<ChildRow>> {
    let renamed: BTreeMap<RecordId, RecordId> = tables
        .values()
        .flatten()
        .map(|r| (r.id, new_ids()))
        .collect();
    tables
        .iter()
        .map(|(name, rows)| (name.clone(), clone_with(rows, &renamed)))
        .collect()
}

fn clone_with(rows: &[ChildRow], renamed: &BTreeMap<RecordId, RecordId>) -> Vec<ChildRow> {
    rows.iter()
        .map(|row| ChildRow {
            // Every row's id is in `renamed` by construction; the fallback keeps this total.
            id: renamed.get(&row.id).copied().unwrap_or(row.id),
            fields: row
                .fields
                .iter()
                .map(|(name, value)| {
                    let value = match value {
                        Value::Id(id) => Value::Id(renamed.get(id).copied().unwrap_or(*id)),
                        other => other.clone(),
                    };
                    (name.clone(), value)
                })
                .collect(),
        })
        .collect()
}

fn check_fresh(table: &str, old: &[ChildRow], cloned: &[ChildRow]) -> AppResult<()> {
    let old_ids: BTreeSet<RecordId> = old.iter().map(|r| r.id).collect();
    let mut seen = BTreeSet::new();
    for (i, row) in cloned.iter().enumerate() {
        if old_ids.contains(&row.id) || !seen.insert(row.id) {
            let number = u32::try_from(i + 1).unwrap_or(u32::MAX);
            return Err(AppError::field(
                FieldPath::row(table, number, "id"),
                INVALID_ROW,
                "A cloned row was given an id that is already in use.",
            ));
        }
    }
    Ok(())
}
