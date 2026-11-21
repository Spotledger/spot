//! Property tests: the diff is stable, `diff(a, a)` is empty, and applying the recorded
//! changes to `before` reproduces `after` (WP-DOC-07.T1).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use proptest::prelude::*;
use spot_activity::diff::{apply_scalar_changes, diff_documents};
use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{Name, Value};

/// A generated scalar value, of one of the shapes the diff handles.
#[derive(Clone, Debug)]
struct Gen {
    kind: u8, // 0 text, 1 int, 2 decimal, 3 id
    text: String,
    int: i64,
    decimal: i64, // cents
    id: u32,
}

fn values() -> impl Strategy<Value = Gen> {
    (
        0u8..4,
        "[a-z]{0,8}",
        any::<i64>(),
        -1_000_000i64..1_000_000,
        0u32..64,
    )
        .prop_map(|(kind, text, int, decimal, id)| Gen {
            kind,
            text,
            int,
            decimal,
            id,
        })
}

fn value_of(g: &Gen) -> Value {
    match g.kind {
        0 => Value::Text(g.text.clone()),
        1 => Value::Int(g.int),
        2 => Value::Decimal(spot_app_api::prelude::Decimal::new(g.decimal, 2)),
        _ => Value::Id(uuid7(1, g.id)),
    }
}

fn fields(gens: &[Gen]) -> std::collections::BTreeMap<Name, Value> {
    let mut f = std::collections::BTreeMap::new();
    for (i, g) in gens.iter().enumerate() {
        let name = match i {
            0 => n("number"),
            1 => n("total"),
            2 => n("remarks"),
            _ => n("posting_date"), // only 3 scalar slots are exercised
        };
        f.insert(name, value_of(g));
        if i >= 2 {
            break;
        }
    }
    f
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn diff_a_a_is_empty(gens in prop::collection::vec(values(), 0..3)) {
        let f = fields(&gens);
        let t = invoice_tables();
        let changes = diff_documents(&f, &t, &f, &t, &meta(), "sales_invoice");
        prop_assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn applying_changes_reproduces_after(
        before_gens in prop::collection::vec(values(), 0..3),
        after_gens in prop::collection::vec(values(), 0..3),
    ) {
        let before = fields(&before_gens);
        let after = fields(&after_gens);
        let t = invoice_tables();
        let changes = diff_documents(&before, &t, &after, &t, &meta(), "sales_invoice");
        let mut applied = before.clone();
        let unapplied = apply_scalar_changes(&mut applied, &changes);
        prop_assert!(unapplied.is_empty(), "{unapplied:?} of {changes:?}");
        prop_assert_eq!(applied, after);
    }

    #[test]
    fn the_diff_is_a_function_of_the_images_only(
        before_gens in prop::collection::vec(values(), 0..3),
        after_gens in prop::collection::vec(values(), 0..3),
    ) {
        let before = fields(&before_gens);
        let after = fields(&after_gens);
        let t = invoice_tables();
        let a = diff_documents(&before, &t, &after, &t, &meta(), "sales_invoice");
        let b = diff_documents(&before, &t, &after, &t, &meta(), "sales_invoice");
        prop_assert_eq!(a, b);
    }
}

#[test]
fn child_row_scalars_apply_by_id_paths_are_not_scalar_changes() {
    // A row change does not apply through `apply_scalar_changes` (its path carries `[id=`);
    // the property above covers scalar fields only, as the brief asks.
    let (r1, _) = ids();
    let mut tb = invoice_tables();
    tb.insert(n("items"), vec![row_with(r1, "a", dec("1"))]);
    let mut ta = invoice_tables();
    ta.insert(n("items"), vec![row_with(r1, "a", dec("2"))]);
    let f = invoice_fields();
    let changes = diff_documents(&f, &tb, &f, &ta, &meta(), "sales_invoice");
    assert_eq!(changes.len(), 1);
    assert!(changes[0].path.contains("[id="));
    let mut f2 = f.clone();
    let unapplied = apply_scalar_changes(&mut f2, &changes);
    assert_eq!(unapplied.len(), 1);
    assert_eq!(f2, f, "nothing applied");
    let _ = ChildRow {
        id: r1,
        fields: Default::default(),
    };
}
