//! Permission awareness: the builder offers only fields its author may read and report on,
//! and a published report still checks every viewer — a column the author could see is
//! removed or masked for a viewer who may not see it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_nocode_builders::{DraftColumn, ReportDraft, offers, plan_for};
use spot_reports::runner::COLUMNS_REMOVED;
use support::{principal, world};

fn memo_draft() -> ReportDraft {
    let mut draft = ReportDraft::new(
        "draft-2",
        "gina",
        "journal_memos",
        "Journal memos",
        "journal",
    );
    draft.columns = vec![
        DraftColumn::new("title").with_label("Journal"),
        DraftColumn::new("memo").with_label("Memo"),
    ];
    draft.limit = Some(100);
    draft.roles = vec![
        "reader".to_owned(),
        "masked".to_owned(),
        "analyst".to_owned(),
    ];
    draft
}

#[tokio::test]
async fn the_palette_offers_only_fields_the_author_may_report_on() {
    let world = world().await;
    let analyst = offers(&world.engine, &world.meta, &principal("gina"), "journal");
    assert!(
        analyst.field("memo").is_some(),
        "the analyst may report on the memo"
    );
    assert!(analyst.field("debit").is_some());
    assert!(
        analyst.field("id").is_some(),
        "a report can always count records"
    );
    assert!(
        analyst.join("account").is_some(),
        "the account link is offered with the fields the analyst may use"
    );

    let reader = offers(&world.engine, &world.meta, &principal("alice"), "journal");
    assert!(
        reader.field("memo").is_none(),
        "a field the reader may not report on is not offered"
    );
    assert!(reader.field("debit").is_some());

    let unknown = offers(&world.engine, &world.meta, &principal("gina"), "missing");
    assert!(unknown.is_empty());

    let writer = offers(&world.engine, &world.meta, &principal("frank"), "journal");
    assert!(writer.field("title").is_none(), "a writer may not report");
}

#[tokio::test]
async fn an_author_cannot_build_over_a_field_they_may_not_report_on() {
    let world = world().await;
    let errs = plan_for(
        &memo_draft(),
        &world.meta_set(),
        &world.catalog,
        &world.engine,
        &principal("alice"),
    )
    .expect_err("the reader may not report on the memo");
    assert!(
        errs.iter()
            .any(|i| i.code == "nocode_builders.field_not_offered"
                && i.path.as_deref() == Some("memo")),
        "{errs:?}"
    );

    plan_for(
        &memo_draft(),
        &world.meta_set(),
        &world.catalog,
        &world.engine,
        &principal("gina"),
    )
    .expect("the analyst may report on the memo");
}

#[tokio::test]
async fn a_viewer_without_the_field_right_loses_the_column_even_though_the_author_saw_it() {
    let world = world().await;
    let plan = plan_for(
        &memo_draft(),
        &world.meta_set(),
        &world.catalog,
        &world.engine,
        &principal("gina"),
    )
    .expect("the analyst's report compiles");
    let args = support::args(serde_json::json!({}));

    let author = world
        .runner()
        .run(&plan.compiled, &principal("gina"), &args, None)
        .await
        .expect("the author runs it");
    assert_eq!(author.result.columns.len(), 2);
    assert_eq!(author.result.rows[0][1], serde_json::json!("memo-0000"));

    let viewer = world
        .runner()
        .run(&plan.compiled, &principal("alice"), &args, None)
        .await
        .expect("the viewer runs it");
    assert_eq!(viewer.result.columns.len(), 1, "the memo column is removed");
    assert_eq!(viewer.result.columns[0].name, "title");
    assert_eq!(viewer.messages.len(), 1);
    assert_eq!(viewer.messages[0].code, COLUMNS_REMOVED);
    assert!(viewer.result.rows.iter().all(|row| row.len() == 1));
}

#[tokio::test]
async fn a_masked_viewer_gets_the_masked_value() {
    let world = world().await;
    let plan = plan_for(
        &memo_draft(),
        &world.meta_set(),
        &world.catalog,
        &world.engine,
        &principal("gina"),
    )
    .expect("compiles");
    let run = world
        .runner()
        .run(
            &plan.compiled,
            &principal("erin"),
            &support::args(serde_json::json!({})),
            None,
        )
        .await
        .expect("the masked viewer runs it");
    assert_eq!(run.result.columns.len(), 2);
    let memo = run.result.rows[0][1].as_str().expect("a masked text");
    assert_ne!(memo, "memo-0000");
    assert_eq!(
        memo,
        spot_permissions::mask("memo-0000", &spot_permissions::MaskFormat::LastChars(2))
    );
}
