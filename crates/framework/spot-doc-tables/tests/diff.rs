//! `diff_rows` and `table_changes`: every kind of difference alone and combined.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_app_api::prelude::Value;
use spot_app_api::{ChildRow, Document};
use spot_doc_tables::{Moved, RowChange, diff_rows, table_changes};

fn moved(i: u32, from: u32, to: u32) -> Moved {
    Moved {
        id: id(i),
        from,
        to,
    }
}

#[test]
fn identical_tables_have_an_empty_diff() {
    let rows = [item(1, "bolt", 1000), item(2, "nut", 2000)];
    assert!(diff_rows(&rows, &rows).is_empty());
    assert!(diff_rows(&[], &[]).is_empty());
    let d = diff_rows(&rows, &rows);
    assert!(!d.is_reordered() && !d.has_row_changes());
}

#[test]
fn rows_added_at_the_end_in_the_middle_and_the_start() {
    let (a, b, c) = (item(1, "a", 1), item(2, "b", 2), item(3, "c", 3));
    let x = item(9, "x", 9);
    let d = diff_rows(&[a.clone(), b.clone()], &[a.clone(), b.clone(), x.clone()]);
    assert_eq!(
        (d.added, d.removed, d.moved.len(), d.changed.len()),
        (vec![x.id], vec![], 0, 0)
    );
    // A row inserted in the middle shifts the later rows' numbers, but nobody's relative order
    // changed: they are not moved.
    let d = diff_rows(
        &[a.clone(), b.clone(), c.clone()],
        &[a.clone(), x.clone(), b.clone(), c.clone()],
    );
    assert_eq!(d.added, vec![x.id]);
    assert!(d.moved.is_empty(), "{:?}", d.moved);
    let d = diff_rows(&[a.clone(), b.clone()], &[x.clone(), a, b]);
    assert!(d.moved.is_empty());
    assert_eq!(d.added, vec![x.id]);
}

#[test]
fn rows_removed_from_the_start_middle_and_end_are_not_moves() {
    let (a, b, c, e) = (
        item(1, "a", 1),
        item(2, "b", 2),
        item(3, "c", 3),
        item(4, "e", 4),
    );
    let all = [a.clone(), b.clone(), c.clone(), e.clone()];
    for removed in [0, 1, 3] {
        let mut new = all.to_vec();
        let gone = new.remove(removed);
        let d = diff_rows(&all, &new);
        assert_eq!(d.removed, vec![gone.id]);
        assert!(d.added.is_empty() && d.moved.is_empty() && d.changed.is_empty());
    }
    // Two removals come back in old order.
    let d = diff_rows(&all, &[b, e]);
    assert_eq!(d.removed, vec![id(1), id(3)]);
    assert!(d.moved.is_empty());
}

#[test]
fn a_reorder_only_is_reordered_not_changed() {
    let (a, b, c) = (item(1, "a", 1), item(2, "b", 2), item(3, "c", 3));
    let d = diff_rows(
        &[a.clone(), b.clone(), c.clone()],
        &[c.clone(), a.clone(), b.clone()],
    );
    assert_eq!(d.moved, vec![moved(3, 3, 1)]);
    assert!(d.is_reordered() && !d.has_row_changes() && !d.is_empty());
    let d = diff_rows(&[a.clone(), b.clone(), c.clone()], &[b, c, a]);
    assert_eq!(
        d.moved,
        vec![moved(1, 1, 3)],
        "the first row went to the end"
    );
    assert!(d.is_reordered());
}

#[test]
fn ties_keep_the_rows_that_come_earliest_in_the_new_table() {
    let (a, b, c, e) = (
        item(1, "a", 1),
        item(2, "b", 2),
        item(3, "c", 3),
        item(4, "e", 4),
    );
    // A swap: B stays (it is first in the new table), A moved.
    let d = diff_rows(&[a.clone(), b.clone()], &[b.clone(), a.clone()]);
    assert_eq!(d.moved, vec![moved(1, 1, 2)]);
    // A full reversal keeps one row, the first of the new table; the other three moved, in new order.
    let d = diff_rows(
        &[a.clone(), b.clone(), c.clone(), e.clone()],
        &[e.clone(), c.clone(), b.clone(), a.clone()],
    );
    assert_eq!(
        d.moved,
        vec![moved(3, 3, 2), moved(2, 2, 3), moved(1, 1, 4)]
    );
    // Moving one row to the front moves only that row.
    let d = diff_rows(&[a.clone(), b.clone(), c.clone(), e.clone()], &[e, a, b, c]);
    assert_eq!(d.moved, vec![moved(4, 4, 1)]);
}

#[test]
fn changed_rows_name_the_differing_fields() {
    let (a, b) = (item(1, "a", 1), item(2, "b", 2));
    let mut a2 = a.clone();
    a2.fields.insert(n("qty"), qty(5));
    a2.fields.insert(n("rate"), Value::Null); // a field present on one side only
    let d = diff_rows(&[a.clone(), b.clone()], &[a2, b.clone()]);
    assert_eq!(
        d.changed,
        vec![RowChange {
            id: id(1),
            fields: vec![n("qty"), n("rate")]
        }]
    );
    assert!(d.added.is_empty() && d.removed.is_empty() && d.moved.is_empty());
    assert!(!d.is_reordered() && d.has_row_changes());
    // A moved and changed row is both.
    let mut b2 = b.clone();
    b2.fields.insert(n("item"), "bb".into());
    let d = diff_rows(&[a.clone(), b], &[b2, a]);
    assert_eq!(d.moved.len(), 1);
    assert_eq!(d.changed.len(), 1);
    assert_eq!(d.changed[0].fields, vec![n("item")]);
    assert!(!d.is_reordered(), "a change makes it more than a reorder");
}

#[test]
fn everything_at_once_is_reported_in_a_fixed_order() {
    let (a, b, c, e) = (
        item(1, "a", 1),
        item(2, "b", 2),
        item(3, "c", 3),
        item(4, "e", 4),
    );
    let mut c2 = c.clone();
    c2.fields.insert(n("qty"), qty(30));
    let (x, y) = (item(8, "x", 8), item(9, "y", 9));
    // old: a b c e   new: y c' a x   (b and e removed, y and x added, c changed, c and a swapped)
    let d = diff_rows(&[a, b, c, e], &[y.clone(), c2, item(1, "a", 1), x.clone()]);
    assert_eq!(d.added, vec![y.id, x.id]);
    assert_eq!(d.removed, vec![id(2), id(4)]);
    assert_eq!(
        d.changed.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![id(3)]
    );
    // Kept rows: a (old 1) and c (old 3); in the new table c comes before a, so a is moved.
    assert_eq!(d.moved, vec![moved(1, 1, 3)]);
}

#[test]
fn rows_are_matched_by_id_not_by_value() {
    let a = item(1, "same", 1);
    let b = item(2, "same", 1);
    let d = diff_rows(std::slice::from_ref(&a), std::slice::from_ref(&b));
    assert_eq!((d.added, d.removed), (vec![b.id], vec![a.id]));
    assert!(d.moved.is_empty() && d.changed.is_empty());
}

#[test]
fn a_repeated_id_counts_once_and_nothing_panics() {
    let (a, b) = (item(1, "a", 1), item(2, "b", 2));
    let dup = [a.clone(), b.clone(), a.clone()];
    let d = diff_rows(&dup, &dup);
    assert!(d.is_empty());
    let d = diff_rows(&dup, &[b, a]);
    assert_eq!(d.moved.len(), 1);
}

#[test]
fn table_changes_lists_only_the_tables_that_differ() {
    let (a, b) = (item(1, "a", 1), item(2, "b", 2));
    let old = loaded(&[a.clone(), b.clone()]);
    assert!(table_changes(&old, &old).is_empty());
    let mut new = old.clone();
    new.rows_mut(n("items")).reverse();
    let changes = table_changes(&old, &new);
    assert_eq!(changes.keys().collect::<Vec<_>>(), [&n("items")]);
    assert!(changes[&n("items")].is_reordered());
    // A table that exists on one side only is compared with an empty table.
    let mut with_taxes = new.clone();
    let mut tax = ChildRow {
        id: id(50),
        fields: Default::default(),
    };
    tax.fields.insert(n("description"), "VAT".into());
    with_taxes.rows_mut(n("taxes")).push(tax);
    let changes = table_changes(&old, &with_taxes);
    assert_eq!(
        changes.keys().collect::<Vec<_>>(),
        [&n("items"), &n("taxes")]
    );
    assert_eq!(changes[&n("taxes")].added, vec![id(50)]);
    let changes = table_changes(&with_taxes, &old);
    assert_eq!(changes[&n("taxes")].removed, vec![id(50)]);
    // An emptied table equals a missing one.
    let mut emptied = old.clone();
    emptied.rows_mut(n("taxes"));
    assert!(table_changes(&old, &emptied).is_empty());
}

#[test]
fn table_changes_agree_with_changed_fields() {
    let (a, b, c) = (item(1, "a", 1), item(2, "b", 2), item(3, "c", 3));
    let old = loaded(&[a.clone(), b.clone(), c.clone()]);
    let table_names = |doc: &Document| -> Vec<spot_app_api::prelude::Name> {
        doc.changed_fields()
            .into_iter()
            .filter(|f| doc.tables().contains_key(f))
            .collect()
    };
    type Edit = Box<dyn Fn(&mut Document)>;
    let edits: Vec<(&str, Edit)> = vec![
        ("nothing", Box::new(|_| {})),
        ("reorder", Box::new(|d| d.rows_mut(n("items")).reverse())),
        (
            "remove",
            Box::new(|d| drop(d.rows_mut(n("items")).remove(1))),
        ),
        (
            "add",
            Box::new(|d| d.rows_mut(n("items")).push(item(7, "x", 7))),
        ),
        (
            "edit",
            Box::new(|d| drop(d.rows_mut(n("items"))[0].fields.insert(n("qty"), qty(99)))),
        ),
        ("clear", Box::new(|d| d.rows_mut(n("items")).clear())),
        (
            "other table",
            Box::new(|d| {
                d.rows_mut(n("taxes")).push(ChildRow {
                    id: id(60),
                    fields: Default::default(),
                })
            }),
        ),
        ("set a field only", Box::new(|d| d.set(n("number"), "SI-1"))),
    ];
    for (label, edit) in edits {
        let mut new = old.clone();
        edit(&mut new);
        let mine: Vec<_> = table_changes(&old, &new).into_keys().collect();
        assert_eq!(mine, table_names(&new), "{label}");
    }
}
