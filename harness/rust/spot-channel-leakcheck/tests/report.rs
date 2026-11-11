//! Channel `Report`: `spot-reports` through `Compiler` and `ReportRunner::run`, the real
//! public entry points, over a fake storage backend.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_field_guard::leakcheck::{Output, assert_no_leak, find_leaks};
use spot_reports::{ReportArgs, ReportError, RunOutput};

/// Everything a caller receives from a run: the column list, every row and every message.
fn serialise(out: &RunOutput) -> Vec<Output> {
    let columns: Vec<_> = out
        .result
        .columns
        .iter()
        .map(|c| format!("{}|{}", c.name, c.label))
        .collect();
    let messages: Vec<_> = out
        .messages
        .iter()
        .map(|m| format!("{} {}", m.code, m.text))
        .collect();
    vec![
        Output::Json(serde_json::to_string(&out.result.rows).unwrap()),
        Output::Text(columns.join("\n")),
        Output::Text(messages.join("\n")),
    ]
}

fn refusal(e: &ReportError) -> Output {
    Output::Text(format!("{} {}", e.code(), e))
}

/// Runs the report as `who` and checks nothing outside what `who` may report leaves, and that
/// what may leave does.
async fn check(w: &World, who: &str) {
    let corpus = corpus();
    let all = [
        "number",
        "notes",
        "card_number",
        "holder",
        "profile_json",
        "salary",
        "annual_cost",
    ];
    let visible = visible_labels(who, true);
    let mut outputs: Vec<Output> = Vec::new();

    match w
        .run(who, &report_toml(&all, ""), ReportArgs::empty())
        .await
    {
        Ok(out) => outputs.extend(serialise(&out)),
        Err(e) => outputs.push(refusal(&e)),
    }

    // Each protected column alone, so partial visibility (the deny, read without report) is
    // told apart column by column.
    for (label, _, path) in SECRETS {
        if path.contains('.') {
            continue; // child-table columns are not report columns of the base DocType
        }
        let one = w
            .run(
                who,
                &report_toml(&["number", path], ""),
                ReportArgs::empty(),
            )
            .await;
        let is_visible = visible.contains(&label);
        match one {
            Ok(out) => {
                let ser = serialise(&out);
                if is_visible {
                    // The positive control: a principal who may report the field sees it.
                    let seen: Vec<String> = find_leaks(&corpus.only(&[label]), &ser)
                        .into_iter()
                        .map(|l| l.secret)
                        .collect();
                    assert!(
                        seen.iter().any(|s| s == label),
                        "{who}: `{label}` should be in the report but is not"
                    );
                }
                outputs.extend(ser);
            }
            Err(e) => {
                assert!(
                    !is_visible,
                    "{who}: `{label}` is visible but the report failed: {e}"
                );
                outputs.push(refusal(&e));
            }
        }
    }
    assert_no_leak(&corpus.without(&visible), &outputs);
}

#[tokio::test]
async fn report_never_leaks() {
    let w = world().await;
    for name in PRINCIPALS {
        check(&w, name).await;
    }
}

/// A principal with the field-level `read` right on `pci` and `hr` but no `report` right (on
/// the DocType or on the group) receives no value in a report: the runner asks the field
/// guard for `Channel::Report`, which needs `report`.
#[tokio::test]
async fn report_read_without_report_right_never_leaks() {
    let w = world().await;
    check(&w, "read_no_report").await;
}

#[tokio::test]
async fn a_removed_column_is_announced_by_count_never_by_value() {
    let w = world().await;
    let out = w
        .run(
            "clerk",
            &report_toml(&["number", "card_number"], ""),
            ReportArgs::empty(),
        )
        .await;
    // `clerk` holds `read` but not `report`: the plain column is removed too, and the run says
    // so without carrying any value.
    if let Ok(out) = out {
        assert_no_leak(&corpus(), &serialise(&out));
    }
}
