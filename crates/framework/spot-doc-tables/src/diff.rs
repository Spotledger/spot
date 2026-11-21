//! What happened to a table's rows between two versions of a document.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use spot_app_api::prelude::{Name, RecordId};
use spot_app_api::{ChildRow, Document};

/// A row that kept its id but is no longer in the same place relative to the other kept rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moved {
    /// The row.
    pub id: RecordId,
    /// Its row number in the old table (from 1).
    pub from: u32,
    /// Its row number in the new table (from 1).
    pub to: u32,
}

/// A row present before and after whose values differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowChange {
    /// The row.
    pub id: RecordId,
    /// The fields whose entries differ, sorted: a value that changed, or a field present on only
    /// one side (a stored `Null` and an absent field are different entries, exactly as
    /// `Document::changed_fields` sees tables).
    pub fields: Vec<Name>,
}

/// The difference between two versions of one table. Rows are matched by id only.
///
/// Every list is in a fixed order: `added` and `changed` by new row number, `removed` by old row
/// number, `moved` by new row number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableDiff {
    /// Ids that exist only in the new table.
    pub added: Vec<RecordId>,
    /// Ids that exist only in the old table.
    pub removed: Vec<RecordId>,
    /// Rows whose relative order changed; see [`diff_rows`] for exactly which rows those are.
    pub moved: Vec<Moved>,
    /// Rows whose values changed (whether or not they also moved).
    pub changed: Vec<RowChange>,
}

impl TableDiff {
    /// Whether the two tables are identical: same ids, same order, same values.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.moved.is_empty()
            && self.changed.is_empty()
    }

    /// Whether rows were only reordered: something moved, and nothing was added, removed or
    /// changed. Such a table is "reordered", not "changed".
    pub fn is_reordered(&self) -> bool {
        !self.moved.is_empty() && !self.has_row_changes()
    }

    /// Whether any row was added, removed or had its values changed.
    pub fn has_row_changes(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty() || !self.changed.is_empty()
    }
}

/// Diffs two versions of a table.
///
/// - **added**: ids in `new` and not in `old`. **removed**: ids in `old` and not in `new`.
/// - **changed**: rows in both whose field entries differ, with the names of the differing fields.
/// - **moved**: among the rows in both, the smallest set whose removal leaves the rest in the same
///   relative order in both tables. Inserting or deleting rows shifts the row numbers of the rows
///   after them, but does not change anyone's relative order, so those rows are *not* moved.
///   When several smallest sets exist (swapping two rows), the rows kept in place are the ones
///   that come **earliest in the new table**: for `[A, B]` becoming `[B, A]`, `B` stays and `A`
///   moved.
///
/// Rows are matched by id only: two rows with equal values and different ids are one removal and
/// one addition. An id that appears twice in a slice (an invalid table, caught by
/// `validate_table_shape`) counts once, at its first position; the diff never panics.
pub fn diff_rows(old: &[ChildRow], new: &[ChildRow]) -> TableDiff {
    let old_rows = first_occurrences(old);
    let new_rows = first_occurrences(new);
    let in_old: HashMap<RecordId, usize> = old_rows.iter().map(|(i, r)| (r.id, *i)).collect();
    let in_new: HashMap<RecordId, usize> = new_rows.iter().map(|(i, r)| (r.id, *i)).collect();

    let added = new_rows
        .iter()
        .filter(|(_, r)| !in_old.contains_key(&r.id))
        .map(|(_, r)| r.id)
        .collect();
    let removed = old_rows
        .iter()
        .filter(|(_, r)| !in_new.contains_key(&r.id))
        .map(|(_, r)| r.id)
        .collect();

    // The rows in both, in new order, each with its old index.
    let common: Vec<(usize, usize, &ChildRow)> = new_rows
        .iter()
        .filter_map(|(new_index, row)| {
            in_old
                .get(&row.id)
                .map(|old_index| (*new_index, *old_index, *row))
        })
        .collect();
    let old_positions: Vec<usize> = common.iter().map(|(_, old_index, _)| *old_index).collect();
    let kept = longest_increasing(&old_positions);

    let mut moved = Vec::new();
    let mut changed = Vec::new();
    for (k, (new_index, old_index, row)) in common.iter().enumerate() {
        if !kept[k] {
            moved.push(Moved {
                id: row.id,
                from: number(*old_index),
                to: number(*new_index),
            });
        }
        let fields = differing_fields(&old[*old_index], row);
        if !fields.is_empty() {
            changed.push(RowChange { id: row.id, fields });
        }
    }
    TableDiff {
        added,
        removed,
        moved,
        changed,
    }
}

/// For every table whose rows differ between `old` and `new`, its diff; none for identical tables.
/// A table present on one side only is compared with an empty table.
///
/// The keys are the table names a change event must list: they agree with the table names in
/// [`Document::changed_fields`] whenever `old` is the document as loaded and `new` the document
/// after the change.
pub fn table_changes(old: &Document, new: &Document) -> BTreeMap<Name, TableDiff> {
    let names: BTreeSet<&Name> = old.tables().keys().chain(new.tables().keys()).collect();
    names
        .into_iter()
        .filter_map(|name| {
            let diff = diff_rows(old.rows(name.as_str()), new.rows(name.as_str()));
            (!diff.is_empty()).then(|| (name.clone(), diff))
        })
        .collect()
}

fn number(index: usize) -> u32 {
    u32::try_from(index + 1).unwrap_or(u32::MAX)
}

/// `(index, row)` for the first row of each id.
fn first_occurrences(rows: &[ChildRow]) -> Vec<(usize, &ChildRow)> {
    let mut seen = BTreeSet::new();
    rows.iter()
        .enumerate()
        .filter(|(_, r)| seen.insert(r.id))
        .collect()
}

/// The names of the fields whose entries differ between two versions of a row.
fn differing_fields(old: &ChildRow, new: &ChildRow) -> Vec<Name> {
    let names: BTreeSet<&Name> = old.fields.keys().chain(new.fields.keys()).collect();
    names
        .into_iter()
        .filter(|name| old.fields.get(*name) != new.fields.get(*name))
        .cloned()
        .collect()
}

/// Which elements of `values` (distinct) are in the longest strictly increasing subsequence that
/// starts earliest: among all longest ones, the one whose indexes are lexicographically smallest.
///
/// `len[i]` is the length of the longest increasing subsequence starting at `i`, found with a
/// binary search over the best starting values per length (`O(n log n)`); the subsequence is then
/// built left to right, taking at each step the first index that can still complete the length.
fn longest_increasing(values: &[usize]) -> Vec<bool> {
    let n = values.len();
    let mut len = vec![0_usize; n];
    // `best[l - 1]` is the largest starting value of an increasing subsequence of length `l` among
    // the elements to the right; it decreases as `l` grows.
    let mut best: Vec<usize> = Vec::new();
    for i in (0..n).rev() {
        let v = values[i];
        // The longest subsequence to the right that starts above `v`.
        let l = best.partition_point(|b| *b > v);
        len[i] = l + 1;
        if l == best.len() {
            best.push(v);
        } else if best[l] < v {
            best[l] = v;
        }
    }
    let mut kept = vec![false; n];
    let (mut need, mut at, mut floor) = (best.len(), 0, None::<usize>);
    while need > 0 {
        while at < n && !(len[at] == need && floor.is_none_or(|f| values[at] > f)) {
            at += 1;
        }
        if at == n {
            break; // unreachable by construction; never loop or index out of range
        }
        kept[at] = true;
        floor = Some(values[at]);
        at += 1;
        need -= 1;
    }
    kept
}
