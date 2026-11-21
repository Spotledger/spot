//! Row paths after a patch, and cloning rows with fresh ids.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::*;
use spot_app_api::prelude::{RecordId, Value};
use spot_app_api::{ChildRow, DocumentPatch, RowPatch};
use spot_doc_tables::{
    clone_rows, clone_rows_checked, clone_tables, path_of, renumber, row_number_of, row_path,
    row_paths,
};

#[test]
fn paths_count_rows_from_one() {
    assert_eq!(row_path("items", 3, "qty").as_str(), "items[3].qty");
    assert_eq!(row_path("items", 1, "item").as_str(), "items[1].item");
    let rows = [item(1, "a", 1), item(2, "b", 2), item(3, "c", 3)];
    let numbers = renumber(&rows);
    assert_eq!(
        numbers.iter().map(|r| r.number).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        numbers.iter().map(|r| r.id).collect::<Vec<_>>(),
        [id(1), id(2), id(3)]
    );
    assert!(renumber(&[]).is_empty());
    assert_eq!(row_number_of(&rows, id(3)), Some(3));
    assert_eq!(row_number_of(&rows, id(9)), None);
}

#[test]
fn paths_follow_the_new_positions_after_a_patch() {
    let (a, b, c) = (item(1, "a", 1), item(2, "b", 2), item(3, "c", 3));
    let mut doc = loaded(&[a.clone(), b.clone(), c.clone()]);
    // The patch deletes the first row and reorders: c, then b.
    let patch = DocumentPatch {
        fields: BTreeMap::new(),
        tables: [(
            n("items"),
            vec![
                RowPatch {
                    id: Some(c.id),
                    fields: BTreeMap::new(),
                },
                RowPatch {
                    id: Some(b.id),
                    fields: BTreeMap::new(),
                },
            ],
        )]
        .into(),
    };
    doc.apply_patch(&patch, RecordId::new_v7).unwrap();
    let rows = doc.rows("items");
    // `b` was row 2 and is now row 2 again; `c` was row 3 and is now row 1; `a` is gone.
    assert_eq!(
        path_of("items", rows, c.id, "qty").unwrap().as_str(),
        "items[1].qty"
    );
    assert_eq!(
        path_of("items", rows, b.id, "qty").unwrap().as_str(),
        "items[2].qty"
    );
    assert_eq!(path_of("items", rows, a.id, "qty"), None);
    let all = row_paths("items", rows, "item");
    assert_eq!(all[&c.id].as_str(), "items[1].item");
    assert_eq!(all[&b.id].as_str(), "items[2].item");
    assert_eq!(all.len(), 2);
    // A repeated id names its first row.
    let dup = [item(1, "a", 1), item(1, "again", 1)];
    assert_eq!(row_number_of(&dup, id(1)), Some(1));
    assert_eq!(row_paths("items", &dup, "qty").len(), 1);
}

fn counter() -> impl FnMut() -> RecordId {
    let mut next = 100;
    move || {
        next += 1;
        id(next)
    }
}

#[test]
fn clones_keep_values_and_order_and_get_fresh_ids() {
    let rows = [item(1, "a", 1), item(2, "b", 2), item(3, "c", 3)];
    let mut calls = 0;
    let mut ids = counter();
    let cloned = clone_rows(&rows, &mut || {
        calls += 1;
        ids()
    });
    assert_eq!(calls, 3, "one id per row");
    assert_eq!(
        cloned.iter().map(|r| r.id).collect::<Vec<_>>(),
        [id(101), id(102), id(103)],
        "ids are used in row order"
    );
    for (old, new) in rows.iter().zip(&cloned) {
        assert_eq!(old.fields, new.fields);
        assert_ne!(old.id, new.id);
    }
    assert!(clone_rows(&[], &mut counter()).is_empty());
}

#[test]
fn nothing_in_a_clone_points_at_an_old_row() {
    // Row 2 refers to row 1 (a "sub-item of" link) and to a customer outside the table.
    let customer = id(777);
    let mut r1 = item(1, "frame", 1);
    r1.fields.insert(n("customer"), Value::Id(customer));
    let mut r2 = item(2, "bolt", 4);
    r2.fields.insert(n("parent_row"), Value::Id(id(1)));
    r2.fields.insert(n("customer"), Value::Id(customer));
    let cloned = clone_rows(&[r1, r2], &mut counter());
    let (new1, new2) = (&cloned[0], &cloned[1]);
    assert_eq!(
        new2.fields[&n("parent_row")],
        Value::Id(new1.id),
        "the link follows the clone"
    );
    assert_eq!(
        new1.fields[&n("customer")],
        Value::Id(customer),
        "links elsewhere are copied as they are"
    );
    assert_eq!(new2.fields[&n("customer")], Value::Id(customer));
    for row in &cloned {
        for value in row.fields.values() {
            assert!(
                !matches!(value, Value::Id(i) if *i == id(1) || *i == id(2)),
                "{value:?}"
            );
        }
    }
}

#[test]
fn checked_clones_refuse_ids_that_are_not_fresh() {
    let rows = [item(1, "a", 1), item(2, "b", 2)];
    let fresh = clone_rows_checked("items", &rows, &mut counter()).unwrap();
    assert_eq!(fresh.len(), 2);
    // A generator that hands back an old id.
    let mut stale = [id(5), id(1)].into_iter();
    let err = clone_rows_checked("items", &rows, &mut || stale.next().unwrap()).unwrap_err();
    assert_eq!(
        problems(&err),
        [("items[2].id".to_owned(), "document.invalid_row".to_owned())]
    );
    // A generator that repeats itself.
    let err = clone_rows_checked("items", &rows, &mut || id(9)).unwrap_err();
    assert_eq!(
        problems(&err),
        [("items[2].id".to_owned(), "document.invalid_row".to_owned())]
    );
}

#[test]
fn whole_documents_clone_with_one_id_mapping() {
    let mut line = item(1, "a", 1);
    line.fields.insert(n("tax_row"), Value::Id(id(50)));
    let mut tax = ChildRow {
        id: id(50),
        fields: Default::default(),
    };
    tax.fields.insert(n("description"), "VAT".into());
    let tables: BTreeMap<_, _> = [(n("items"), vec![line]), (n("taxes"), vec![tax])].into();
    let cloned = clone_tables(&tables, &mut counter());
    // Tables are processed in name order (items, then taxes), rows in order.
    assert_eq!(cloned[&n("items")][0].id, id(101));
    assert_eq!(cloned[&n("taxes")][0].id, id(102));
    assert_eq!(
        cloned[&n("items")][0].fields[&n("tax_row")],
        Value::Id(id(102)),
        "a link across tables follows"
    );
    assert_eq!(cloned.len(), 2);
    assert!(clone_tables(&BTreeMap::new(), &mut counter()).is_empty());
}
