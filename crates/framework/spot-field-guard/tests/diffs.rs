//! Version-history and audit diffs: both sides of a masked change are masked, a hidden
//! field's change is dropped but the history says something is missing, a derived field
//! follows its inherited groups, and a whole-row snapshot is redacted column by column.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_activity::{FieldChange, REDACTED};
use spot_app_api::prelude::{Decimal, Value};
use spot_field_guard::leakcheck::{Corpus, Output, Secret, assert_no_leak};

fn text(s: &str) -> Option<Value> {
    Some(Value::Text(s.to_owned()))
}

fn change(path: &str, before: Option<Value>, after: Option<Value>) -> FieldChange {
    FieldChange {
        path: path.to_owned(),
        before,
        after,
    }
}

fn diff(who: &str, changes: &[FieldChange]) -> spot_field_guard::RedactedDiff {
    fixture()
        .guard
        .redact_diff(&principal(who), DOCTYPE, &doc_facts(), changes)
        .unwrap()
}

const OLD_CARD: &str = "4111 1111 1111 1111";
const NEW_CARD: &str = "5500 0000 0000 0004";

fn card_change() -> FieldChange {
    change("card_number", text(OLD_CARD), text(NEW_CARD))
}

#[test]
fn both_sides_of_a_masked_change_are_masked() {
    let d = diff("masked_reader", &[card_change()]);
    assert!(!d.hidden_changes);
    assert_eq!(d.changes.len(), 1);
    assert_eq!(d.changes[0].before, text("***************1111"));
    assert_eq!(d.changes[0].after, text("***************0004"));
    let out = Output::Json(serde_json::to_string(&format!("{:?}", d.changes)).unwrap());
    let corpus = Corpus::new()
        .with(Secret::whole("old", OLD_CARD))
        .with(Secret::whole("new", NEW_CARD));
    assert_no_leak(&corpus, &[out]);
}

#[test]
fn a_readable_change_passes_through_untouched() {
    let d = diff("analyst", &[card_change()]);
    assert_eq!(d.changes, vec![card_change()]);
    assert!(!d.hidden_changes);
}

#[test]
fn a_hidden_change_is_dropped_but_the_history_says_so() {
    let number = change("number", text("INV-1"), text("INV-2"));
    let d = diff("clerk", &[number.clone(), card_change()]);
    assert_eq!(d.changes, vec![number]);
    assert!(
        d.hidden_changes,
        "the reader must know something changed that they cannot see"
    );
    let none = diff("nobody", &[card_change()]);
    assert!(none.changes.is_empty() && none.hidden_changes);
}

#[test]
fn a_diff_of_a_derived_field_follows_the_inherited_groups() {
    let annual = change("annual_cost", text("900000.00"), text("1014009.00"));
    // read_no_report has `hr` (the field's own group) but not the inherited `finance`.
    let d = diff("read_no_report", std::slice::from_ref(&annual));
    assert!(d.changes.is_empty() && d.hidden_changes);
    assert_eq!(
        diff("analyst", std::slice::from_ref(&annual)).changes,
        vec![annual.clone()]
    );
    let masked = diff("masked_reader", &[annual]);
    assert_eq!(masked.changes[0].before, text("***"));
    assert_eq!(masked.changes[0].after, text("***"));
}

#[test]
fn a_masked_value_without_a_text_form_becomes_the_redaction_marker() {
    let bonus = change(
        "bonus",
        Some(Value::Decimal(Decimal::new(100_000, 2))),
        Some(Value::Decimal(Decimal::new(150_050, 2))),
    );
    let d = diff("masked_reader", &[bonus]);
    assert_eq!(d.changes[0].before, text(REDACTED));
    assert_eq!(d.changes[0].after, text(REDACTED));
    // An absent side stays absent: setting a value for the first time is still visible.
    let set = change("card_number", None, text(NEW_CARD));
    assert_eq!(diff("masked_reader", &[set]).changes[0].before, None);
}

#[test]
fn a_child_column_follows_its_own_doctype() {
    let row = "items[id=0190c0a0-0000-7000-8000-000000000001]";
    let iban = change(
        &format!("{row}.iban"),
        text("GB82WEST12345698765432"),
        text("DE89370400440532013000"),
    );
    let qty = change(
        &format!("{row}.qty"),
        Some(Value::Int(1)),
        Some(Value::Int(2)),
    );
    let masked = diff("masked_reader", &[iban.clone(), qty.clone()]);
    assert_eq!(masked.changes[0].before, text("******************5432"));
    assert_eq!(masked.changes[1], qty);
    let clerk = diff("clerk", &[iban.clone(), qty.clone()]);
    assert_eq!(clerk.changes, vec![qty]);
    assert!(clerk.hidden_changes);
    // A table the reader cannot see takes its rows with it.
    assert!(diff("nobody", &[iban]).changes.is_empty());
}

#[test]
fn a_whole_row_snapshot_is_redacted_column_by_column() {
    let snapshot = r#"{"id":"0190c0a0-0000-7000-8000-000000000001","fields":{"qty":"2.000","iban":"GB82WEST12345698765432","memo":"4929 1234"}}"#;
    let added = change(
        "items[id=0190c0a0-0000-7000-8000-000000000001]",
        None,
        Some(Value::Json(snapshot.to_owned())),
    );
    let parse = |d: &spot_field_guard::RedactedDiff| -> serde_json::Value {
        match d.changes[0].after.as_ref().unwrap() {
            Value::Json(t) => serde_json::from_str(t).unwrap(),
            other => panic!("not a row snapshot: {other:?}"),
        }
    };
    let masked = diff("masked_reader", std::slice::from_ref(&added));
    let fields = parse(&masked)["fields"].clone();
    assert_eq!(fields["qty"], "2.000");
    assert_eq!(fields["iban"], "******************5432");
    assert_eq!(fields["memo"], "***");
    assert!(!masked.hidden_changes);

    let clerk = diff("clerk", std::slice::from_ref(&added));
    let fields = parse(&clerk)["fields"].clone();
    assert_eq!(fields["qty"], "2.000");
    assert!(fields.get("iban").is_none() && fields.get("memo").is_none());
    assert!(clerk.hidden_changes);

    let leaky = Corpus::new()
        .with(Secret::whole("iban", "GB82WEST12345698765432"))
        .with(Secret::whole("memo", "4929 1234"));
    for d in [&masked, &clerk] {
        let text = serde_json::to_string(&format!("{:?}", d.changes)).unwrap();
        assert_no_leak(&leaky, &[Output::Json(text)]);
    }

    // A snapshot that does not parse is dropped whole rather than passed through.
    let broken = change("items[id=x]", None, Some(Value::Json("not json".into())));
    let d = diff("analyst", &[broken]);
    assert!(d.changes.is_empty() && d.hidden_changes);
}
