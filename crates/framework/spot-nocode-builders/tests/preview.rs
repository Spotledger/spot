//! Preview: the row cap holds, an unusable budget is refused, the time cap is the host's to
//! measure, and the canvas' sample rows are deterministic.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::json;
use spot_nocode_builders::{
    DraftColumn, PreviewBudget, ReportDraft, check_time, preview, sample_rows,
};
use support::{principal, world};

fn draft() -> ReportDraft {
    let mut draft = ReportDraft::new("draft-p", "gina", "journal_list", "Journals", "journal");
    draft.columns = vec![DraftColumn::new("title").with_label("Journal")];
    draft.roles = vec!["reader".to_owned(), "analyst".to_owned()];
    draft
}

#[tokio::test]
async fn a_preview_is_held_to_its_row_cap() {
    let world = world().await;
    let plan = support::compile(&world, &draft()).expect("compiles");
    let budget = PreviewBudget::new(3, 5_000);
    let run = preview(
        &world.runner(),
        &plan.compiled,
        &principal("gina"),
        &support::args(json!({})),
        &budget,
    )
    .await
    .expect("the preview runs");
    assert_eq!(run.cap, 3);
    assert_eq!(run.output.result.rows.len(), 3);
    assert!(
        run.output.result.truncated,
        "there are more rows than the cap"
    );
}

#[tokio::test]
async fn a_preview_never_exceeds_the_reports_own_limit() {
    let world = world().await;
    let mut draft = draft();
    draft.limit = Some(2);
    draft.max_rows = Some(2);
    let plan = support::compile(&world, &draft).expect("compiles");
    let budget = PreviewBudget::new(50, 5_000);
    let run = preview(
        &world.runner(),
        &plan.compiled,
        &principal("gina"),
        &support::args(json!({})),
        &budget,
    )
    .await
    .expect("the preview runs");
    assert_eq!(run.cap, 2);
    assert_eq!(run.output.result.rows.len(), 2);
}

#[test]
fn a_budget_with_no_cap_is_refused() {
    let err = PreviewBudget::new(0, 5_000)
        .check()
        .expect_err("no row cap");
    assert_eq!(err.code, "nocode_builders.preview_budget");
    let err = PreviewBudget::new(10, 0).check().expect_err("no time cap");
    assert_eq!(err.code, "nocode_builders.preview_budget");
    PreviewBudget::canvas()
        .check()
        .expect("the canvas budget is usable");
}

#[test]
fn a_slow_preview_is_stopped() {
    let budget = PreviewBudget::new(10, 1_000);
    check_time(&budget, 1_000).expect("exactly the budget is allowed");
    let err = check_time(&budget, 1_001).expect_err("one millisecond over");
    assert_eq!(err.code, "nocode_builders.preview_too_slow");
}

#[test]
fn sample_rows_are_deterministic_and_shaped_by_the_column() {
    let draft = draft();
    let rows = sample_rows(&draft, 3);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].len(), 1);
    assert_eq!(rows[0][0], json!("title-1"));
    assert_eq!(rows, sample_rows(&draft, 3));
    assert_ne!(rows[0], rows[2]);
}
