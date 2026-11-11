//! Channel `Export`: `spot-bulk-data` through `ExportPlan::build` and `ExportStream`, the
//! real public entry points. Every byte of every part (CSV and NDJSON) goes to the harness.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bulk_data::{BulkError, Format};
use spot_field_guard::leakcheck::{Output, assert_no_leak, find_leaks};

/// One export as the named principal (see `common::run_export`).
async fn run(
    core: &Core,
    store: &Store,
    principal_name: &str,
    format: Format,
    parent: &[&str],
    child: &[&str],
) -> Result<String, BulkError> {
    run_export(
        core,
        store,
        &principal(principal_name),
        format,
        parent,
        child,
    )
    .await
}

/// The refusal as it would reach a caller: its stable code and its message.
fn refusal(e: &BulkError) -> Output {
    Output::Text(format!("{} {}", e.code(), e.message()))
}

#[tokio::test]
async fn export_never_leaks() {
    let core = core();
    let corpus = corpus();
    let store = store(&core.meta, &corpus, 3).await;
    let mut cells = 0;
    for format in [Format::Csv, Format::Ndjson] {
        for name in PRINCIPALS {
            let visible = visible_labels(name, true);
            let forbidden = corpus.without(&visible);
            let mut outputs: Vec<Output> = Vec::new();

            // The everything-request, the way a careless caller asks.
            let all = run(&core, &store, name, format, &PARENT_COLUMNS, &CHILD_COLUMNS).await;
            match &all {
                Ok(text) => outputs.push(Output::Csv(text.clone())),
                Err(e) => {
                    assert!(
                        matches!(e.code(), "permission.field_denied" | "bulk.export_denied"),
                        "{name}: unexpected refusal {}",
                        e.code()
                    );
                    outputs.push(refusal(e));
                }
            }

            // Each protected column alone: allowed or refused by name, never leaked.
            for (label, _, path) in SECRETS {
                let (parent, child): (Vec<&str>, Vec<&str>) = match path.split_once('.') {
                    None => (vec!["id", path], vec![]),
                    Some((_, column)) => (vec!["id"], vec![column]),
                };
                let one = run(&core, &store, name, format, &parent, &child).await;
                let is_visible = visible.contains(&label);
                match one {
                    Ok(text) => {
                        let out = [Output::Csv(text)];
                        if is_visible {
                            // The positive control: a principal who may report the field gets
                            // the value, so a clean result cannot be an empty one.
                            let seen: Vec<String> = find_leaks(&corpus.only(&[label]), &out)
                                .into_iter()
                                .map(|l| l.secret)
                                .collect();
                            assert!(
                                seen.iter().any(|s| s == label),
                                "{name} {format:?}: `{label}` should be exported but is not"
                            );
                        }
                        outputs.extend(out);
                    }
                    Err(e) => {
                        assert!(
                            !is_visible,
                            "{name} {format:?}: `{label}` is visible but its export was refused ({})",
                            e.code()
                        );
                        outputs.push(refusal(&e));
                    }
                }
                cells += 1;
            }

            // A plain column follows the DocType right; it is never protected.
            let plain = run(&core, &store, name, format, &["id", "number"], &[]).await;
            match plain {
                Ok(text) => {
                    assert!(
                        plain_visible(name, true),
                        "{name}: plain export allowed unexpectedly"
                    );
                    if name != "nobody" {
                        assert!(text.contains("INV-0001"), "{name}: plain value missing");
                    }
                    outputs.push(Output::Csv(text));
                }
                Err(e) => outputs.push(refusal(&e)),
            }

            assert_no_leak(&forbidden, &outputs);
        }
    }
    assert!(cells >= 2 * PRINCIPALS.len() * SECRETS.len());
}

#[tokio::test]
async fn an_export_of_a_denied_group_is_refused_by_name_not_dropped() {
    let core = core();
    let corpus = corpus();
    let store = store(&core.meta, &corpus, 1).await;
    // `no_cards` holds an explicit deny on `pci`: asking for the column is refused, and the
    // refusal names the column but never carries a value.
    let err = run(
        &core,
        &store,
        "no_cards",
        Format::Csv,
        &["id", "card_number"],
        &[],
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), "permission.field_denied");
    assert!(err.message().contains("card_number"));
    assert_no_leak(&corpus, &[refusal(&err)]);
    // The same principal exports a group it was not denied.
    let ok = run(
        &core,
        &store,
        "no_cards",
        Format::Csv,
        &["id", "salary"],
        &[],
    )
    .await
    .unwrap();
    assert!(ok.contains(corpus.get("salary").unwrap().value()));
}
