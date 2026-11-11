//! Properties: a random patch, applied and then diffed, agrees with what the patch meant; `diff(a,
//! a)` is empty; moves are minimal; paths always name the new positions.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use proptest::prelude::*;
use spot_app_api::prelude::RecordId;
use spot_app_api::{ChildRow, DocumentPatch, RowPatch};
use spot_doc_tables::{diff_rows, renumber, row_paths, table_changes};

/// Old rows `1..=n`, each with a distinct `qty`.
fn old_rows(n_rows: u32) -> Vec<ChildRow> {
    (1..=n_rows)
        .map(|i| item(i, &format!("row{i}"), i64::from(i)))
        .collect()
}

/// One entry of a patch.
#[derive(Clone, Debug)]
enum Entry {
    /// An old row, by its number, with an optional new quantity.
    Keep(u32, Option<i64>),
    /// A new row with a client-chosen id (`2000 + n`) or none.
    New(Option<u32>),
}

fn entries() -> impl Strategy<Value = Vec<Entry>> {
    let keep = (1_u32..=8, prop::option::of(100_i64..110)).prop_map(|(i, q)| Entry::Keep(i, q));
    let new = prop::option::of(0_u32..6).prop_map(Entry::New);
    prop::collection::vec(prop_oneof![3 => keep, 1 => new], 0..12)
}

/// Longest common subsequence length (the brute-force reference for "moved is minimal").
fn lcs(a: &[RecordId], b: &[RecordId]) -> usize {
    let mut table = vec![vec![0_usize; b.len() + 1]; a.len() + 1];
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            table[i][j] = if a[i - 1] == b[j - 1] {
                table[i - 1][j - 1] + 1
            } else {
                table[i - 1][j].max(table[i][j - 1])
            };
        }
    }
    table[a.len()][b.len()]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn a_table_never_differs_from_itself(n_rows in 0_u32..12) {
        let rows = old_rows(n_rows);
        prop_assert!(diff_rows(&rows, &rows).is_empty());
        prop_assert!(table_changes(&loaded(&rows), &loaded(&rows)).is_empty());
    }

    /// Apply a random patch, diff the result against the original, and compare with what the patch
    /// said: rows kept, rows new, rows gone, rows edited, rows moved (minimally).
    #[test]
    fn the_diff_agrees_with_the_patch(n_rows in 0_u32..=8, patch_entries in entries()) {
        let old = old_rows(n_rows);
        let mut doc = loaded(&old);
        // The patch: duplicates of an old row or of a client id are invalid; keep the first.
        let mut seen: BTreeSet<RecordId> = BTreeSet::new();
        let mut rows = Vec::new();
        let mut kept: Vec<(RecordId, Option<i64>)> = Vec::new();
        let mut client_new: Vec<RecordId> = Vec::new();
        let mut anonymous_new = 0_usize;
        for entry in &patch_entries {
            match entry {
                Entry::Keep(i, q) if *i <= n_rows => {
                    if !seen.insert(id(*i)) { continue; }
                    let mut fields = BTreeMap::new();
                    if let Some(q) = q { fields.insert(n("qty"), qty(*q)); }
                    rows.push(RowPatch { id: Some(id(*i)), fields });
                    kept.push((id(*i), *q));
                }
                // An old row that does not exist is a new row with a client-chosen id.
                Entry::Keep(i, _) => {
                    let new_id = id(2_000 + *i);
                    if !seen.insert(new_id) { continue; }
                    rows.push(RowPatch { id: Some(new_id), fields: BTreeMap::new() });
                    client_new.push(new_id);
                }
                Entry::New(Some(k)) => {
                    let new_id = id(3_000 + *k);
                    if !seen.insert(new_id) { continue; }
                    rows.push(RowPatch { id: Some(new_id), fields: BTreeMap::new() });
                    client_new.push(new_id);
                }
                Entry::New(None) => {
                    rows.push(RowPatch { id: None, fields: BTreeMap::new() });
                    anonymous_new += 1;
                }
            }
        }
        let patch = DocumentPatch { fields: BTreeMap::new(), tables: [(n("items"), rows)].into() };
        let mut fresh = 10_000;
        doc.apply_patch(&patch, || { fresh += 1; id(fresh) }).unwrap();
        let new = doc.rows("items");

        let before = loaded(&old);
        let diff = diff_rows(&old, new);

        // Added: exactly the rows that are new, in the new table's order; removed: the old rows
        // the patch did not mention, in the old table's order.
        let expected_added: Vec<RecordId> = new.iter().map(|r| r.id).filter(|i| !old.iter().any(|o| o.id == *i)).collect();
        prop_assert_eq!(&diff.added, &expected_added);
        prop_assert_eq!(diff.added.len(), client_new.len() + anonymous_new);
        let kept_ids: BTreeSet<RecordId> = kept.iter().map(|(i, _)| *i).collect();
        let expected_removed: Vec<RecordId> = old.iter().map(|r| r.id).filter(|i| !kept_ids.contains(i)).collect();
        prop_assert_eq!(&diff.removed, &expected_removed);

        // Changed: exactly the kept rows given a different quantity.
        let expected_changed: Vec<RecordId> = new
            .iter()
            .filter(|r| kept.iter().any(|(i, q)| *i == r.id && q.is_some_and(|q| qty(q) != old.iter().find(|o| o.id == *i).unwrap().fields[&n("qty")])))
            .map(|r| r.id)
            .collect();
        prop_assert_eq!(diff.changed.iter().map(|c| c.id).collect::<Vec<_>>(), expected_changed);
        for change in &diff.changed {
            prop_assert_eq!(&change.fields, &vec![n("qty")]);
        }

        // Moved: only kept rows; and minimal (the kept rows minus the moved ones are in the same
        // relative order in both tables, and no smaller set achieves that).
        let in_new_order: Vec<RecordId> = new.iter().map(|r| r.id).filter(|i| kept_ids.contains(i)).collect();
        let in_old_order: Vec<RecordId> = old.iter().map(|r| r.id).filter(|i| kept_ids.contains(i)).collect();
        let moved: BTreeSet<RecordId> = diff.moved.iter().map(|m| m.id).collect();
        prop_assert!(moved.is_subset(&kept_ids));
        prop_assert_eq!(moved.len(), in_new_order.len() - lcs(&in_old_order, &in_new_order));
        let stay_old: Vec<RecordId> = in_old_order.iter().copied().filter(|i| !moved.contains(i)).collect();
        let stay_new: Vec<RecordId> = in_new_order.iter().copied().filter(|i| !moved.contains(i)).collect();
        prop_assert_eq!(stay_old, stay_new);
        // The row numbers in `moved` are the real ones, from 1.
        for m in &diff.moved {
            prop_assert_eq!(old[(m.from - 1) as usize].id, m.id);
            prop_assert_eq!(new[(m.to - 1) as usize].id, m.id);
        }
        // `moved` is ordered by new row number.
        prop_assert!(diff.moved.windows(2).all(|w| w[0].to < w[1].to));

        // The diff is empty exactly when the table is unchanged, and `table_changes` agrees with
        // `Document::changed_fields` about whether the table changed.
        prop_assert_eq!(diff.is_empty(), old == new);
        let changes = table_changes(&before, &doc);
        let listed = doc.changed_fields().contains(&n("items"));
        prop_assert_eq!(changes.contains_key(&n("items")), listed);

        // Paths name the new positions.
        let paths = row_paths("items", new, "qty");
        for (i, row) in renumber(new).iter().enumerate() {
            prop_assert_eq!(row.number as usize, i + 1);
            prop_assert_eq!(paths[&row.id].as_str(), format!("items[{}].qty", i + 1));
        }
    }

    /// Reversing the arguments swaps `added` and `removed`.
    #[test]
    fn a_diff_reversed_swaps_additions_and_removals(n_rows in 0_u32..=8, keep in prop::collection::vec(1_u32..=8, 0..8), extra in 0_u32..4) {
        let old = old_rows(n_rows);
        let mut seen = BTreeSet::new();
        let mut new: Vec<ChildRow> = keep.iter().filter(|i| **i <= n_rows && seen.insert(**i)).map(|i| item(*i, "k", i64::from(*i))).collect();
        new.extend((0..extra).map(|k| item(500 + k, "n", 1)));
        let forward = diff_rows(&old, &new);
        let backward = diff_rows(&new, &old);
        let to_set = |v: &[RecordId]| v.iter().copied().collect::<BTreeSet<_>>();
        prop_assert_eq!(to_set(&forward.added), to_set(&backward.removed));
        prop_assert_eq!(to_set(&forward.removed), to_set(&backward.added));
        prop_assert_eq!(forward.moved.len(), backward.moved.len());
    }
}
