//! The kit against the fake app: the good variant passes, each broken one fails with its code.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod fake_app;

use fake_app::{Tamper, Variant, factory, factory_with, scenarios};
use spot_effect_kit::{Report, codes, run};

async fn report(variant: Variant) -> Report {
    run(&factory(variant), &scenarios()).await.unwrap()
}

#[tokio::test]
async fn the_good_app_passes_every_check_on_both_doctypes() {
    let r = report(Variant::Good).await;
    r.assert_clean();
    assert_eq!(r.scenarios, 2);
}

#[tokio::test]
async fn a_missing_reversal_leaves_the_store_unbalanced() {
    let r = report(Variant::MissingReversal).await;
    assert_eq!(r.codes(), [codes::NOT_NETTED]);
    let v = &r.violations[0];
    assert_eq!(v.step.as_ref().unwrap().as_str(), "invoicing:book_cancel");
    assert_eq!(v.store.as_ref().unwrap().as_str(), fake_app::STORE);
    assert_eq!(v.delta.as_deref(), Some("sum +125.00, rows +1"));
    assert!(
        v.to_string().contains("add the reversing step to cancel"),
        "{v}"
    );
    // Both DocTypes fail, and `correct` fails for the same reason.
    assert!(
        r.violations
            .iter()
            .any(|v| v.doctype == "ledger_payment" && v.operation == "cancel")
    );
    assert!(r.violations.iter().any(|v| v.operation == "correct"));
}

#[tokio::test]
async fn an_undeclared_write_is_found_by_diffing_the_store_against_the_receipt() {
    let r = report(Variant::UndeclaredWrite).await;
    assert_eq!(r.codes(), [codes::UNREPORTED_WRITE]);
    let v = &r.violations[0];
    assert_eq!(v.operation, "post");
    assert_eq!(v.step.as_ref().unwrap().as_str(), "invoicing:book_post");
    assert_eq!(v.delta.as_deref(), Some("sum +125.00, rows +1"));
    assert!(
        v.to_string().contains("report every row the step writes"),
        "{v}"
    );
}

#[tokio::test]
async fn an_over_reversal_is_named_as_such() {
    let r = report(Variant::OverReversal).await;
    assert_eq!(r.codes(), [codes::OVER_REVERSAL]);
    let v = &r.violations[0];
    assert_eq!(v.step.as_ref().unwrap().as_str(), "invoicing:book_cancel");
    assert_eq!(v.delta.as_deref(), Some("sum -125.00, rows +2"));
}

#[tokio::test]
async fn deleting_a_row_to_net_to_zero_is_refused() {
    let r = report(Variant::DeletesRow).await;
    assert_eq!(r.codes(), [codes::ROW_DELETED]);
    let v = &r.violations[0];
    assert_eq!(v.operation, "cancel");
    assert_eq!(v.delta.as_deref(), Some("rows -1"));
    assert!(v.to_string().contains("append-only"), "{v}");
}

#[tokio::test]
async fn a_scenario_whose_draft_is_invalid_reports_the_failed_operation() {
    use spot_app_api::Document;
    use spot_effect_kit::Scenario;
    let bad = Scenario::new("ledger_invoice", scenarios()[0].invocation.clone(), || {
        Document::new(
            spot_app_api::prelude::Name::new("ledger_invoice").unwrap(),
            spot_app_api::prelude::RecordId::new_v7(),
        )
    });
    let r = run(&factory(Variant::Good), &[bad]).await.unwrap();
    assert_eq!(r.codes(), [codes::OPERATION_FAILED]);
}

#[tokio::test]
async fn a_reversal_that_does_not_reference_the_posted_row_is_flagged() {
    let r = report(Variant::Unlinked).await;
    assert_eq!(r.codes(), [codes::REVERSAL_UNLINKED]);
    assert_eq!(
        r.violations[0].step.as_ref().unwrap().as_str(),
        "invoicing:book_cancel"
    );
}

#[tokio::test]
async fn a_report_no_declaration_covers_is_flagged_with_the_step() {
    let r = report(Variant::WrongClass).await;
    assert!(r.codes().contains(&codes::UNDECLARED_REPORT), "{r}");
    let v = r
        .violations
        .iter()
        .find(|v| v.code == codes::UNDECLARED_REPORT)
        .unwrap();
    assert_eq!(v.step.as_ref().unwrap().as_str(), "invoicing:book_post");
    assert!(
        v.to_string().contains("declare the effect on the step"),
        "{v}"
    );
    // The store also gained a row that no declared effect of the step accounts for.
    assert!(r.codes().contains(&codes::UNREPORTED_WRITE), "{r}");
}

#[tokio::test]
async fn a_second_cancel_that_is_accepted_is_flagged_and_so_is_its_duplicate_row() {
    let r = run(
        &factory_with(Variant::Good, Tamper::AcceptSecondCancel),
        &scenarios(),
    )
    .await
    .unwrap();
    assert_eq!(
        r.codes(),
        [codes::CANCEL_DUPLICATED, codes::CANCEL_NOT_REFUSED]
    );
}

#[tokio::test]
async fn correct_that_returns_no_linked_draft_is_flagged() {
    let r = run(
        &factory_with(Variant::Good, Tamper::CorrectIsJustCancel),
        &scenarios(),
    )
    .await
    .unwrap();
    assert_eq!(r.codes(), [codes::CORRECT_BROKEN]);
    assert_eq!(r.violations[0].operation, "correct");
}

#[tokio::test]
async fn a_cancel_receipt_that_names_no_rows_is_flagged_when_the_probe_records_links() {
    let r = report(Variant::CancelWithoutRefs).await;
    assert_eq!(r.codes(), [codes::REVERSAL_UNLINKED]);
    let v = &r.violations[0];
    assert_eq!(v.step.as_ref().unwrap().as_str(), "invoicing:book_cancel");
    assert!(v.what.contains("receipt"), "{v}");
}

#[tokio::test]
async fn a_correct_draft_that_already_wrote_a_store_row_is_flagged() {
    let r = run(
        &factory_with(Variant::Good, Tamper::CorrectDraftWritesRows),
        &scenarios(),
    )
    .await
    .unwrap();
    assert_eq!(r.codes(), [codes::CORRECT_BROKEN]);
    let v = &r.violations[0];
    assert_eq!(v.operation, "correct");
    assert_eq!(v.store.as_ref().unwrap().as_str(), fake_app::STORE);
    assert_eq!(v.delta.as_deref(), Some("sum +1.00, rows +1"));
}
