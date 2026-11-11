//! No-inference (ADR-070 section 12 rule 7): a filter, sort, group or aggregate on a field the
//! caller cannot read unmasked is refused on the producers that take a user query, exactly as
//! a field that does not exist is, and the refusal costs the same work. The positive control
//! is the same query as a principal who may read the field.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use spot_bulk_data::{
    AllowAllExport, EXPORT_ALIAS, ExportPlan, ExportSpec, FieldSet, Format, OrderKey,
};
use spot_field_guard::leakcheck::{Output, assert_no_leak};
use spot_reports::ReportArgs;
use spot_storage_contract::{CmpOp, Expr, Value};

fn export_filter(field: &str, value: &str) -> Expr {
    Expr::Cmp(
        Box::new(Expr::field(&name(EXPORT_ALIAS), &name(field))),
        CmpOp::Eq,
        Box::new(Expr::Value(Value::Text(value.into()))),
    )
}

fn plan_with(
    core: &Core,
    who: &str,
    spec: &ExportSpec,
) -> Result<ExportPlan, spot_bulk_data::BulkError> {
    let mut fields = FieldSet::new(DOCTYPE, "owner", &PARENT_COLUMNS);
    fields.fields.push("no_such_field".to_owned());
    ExportPlan::build(
        spec,
        &principal(who),
        &core.guard,
        &AllowAllExport,
        &fields,
        &*core.protection,
    )
}

/// Whether the principal may read a field of `group` **unmasked** (the no-inference rule
/// asks for the read right, not the report right).
fn may_read(who: &str, group: &str) -> bool {
    groups_seen(who, false).contains(&group)
}

/// The principals that may not read `card_number` unmasked.
fn unauthorised() -> Vec<&'static str> {
    PRINCIPALS
        .into_iter()
        .filter(|p| !may_read(p, "pci"))
        .collect()
}

#[test]
fn an_export_filter_or_sort_on_a_protected_field_is_refused() {
    let core = core();
    let secret = corpus().get("card").unwrap().value().to_owned();
    for who in unauthorised() {
        let filter = ExportSpec::csv(DOCTYPE)
            .with_columns(&["id", "number"])
            .with_filter(export_filter("card_number", &secret));
        let sort = ExportSpec::csv(DOCTYPE)
            .with_columns(&["id", "number"])
            .with_order(vec![OrderKey::asc("card_number")]);
        for spec in [filter, sort] {
            // Refused (or, for the principal with no export-level rights, refused earlier):
            // never a plan, and the refusal never carries the value being probed.
            let err = plan_with(&core, who, &spec).unwrap_err();
            if matches!(who, "masked_reader" | "no_cards") {
                // These pass the column check on `id`, so the refusal is the query's alone.
                assert_eq!(err.code(), "permission.field_denied", "{who}");
                assert!(err.message().contains("card_number"), "{who}");
            }
            assert_no_leak(
                &corpus(),
                &[Output::Text(format!("{} {}", err.code(), err.message()))],
            );
        }
    }
}

#[test]
fn the_same_export_query_is_allowed_for_a_reader_of_the_field() {
    let core = core();
    let secret = corpus().get("card").unwrap().value().to_owned();
    for who in ["analyst", "admin"] {
        let spec = ExportSpec::csv(DOCTYPE)
            .with_columns(&["id", "number"])
            .with_filter(export_filter("card_number", &secret))
            .with_order(vec![OrderKey::asc("card_number")]);
        assert!(
            plan_with(&core, who, &spec).is_ok(),
            "{who} may filter and sort"
        );
    }
}

/// The work a protected-field refusal costs must not differ from a query on a field that does
/// not exist (which the plan lets through; the backend fails it later): over 100 samples the
/// median times stay within a stated tolerance (a factor of 8 or 2 ms), so timing says
/// nothing about the protection.
/// `Instant` is monotonic and measures elapsed time only; no wall-clock value is read.
#[test]
fn the_refusal_costs_the_same_for_a_protected_and_a_missing_field() {
    const SAMPLES: usize = 100;
    let core = core();
    let who = "masked_reader";
    let time = |field: &str| -> Duration {
        let spec = ExportSpec::csv(DOCTYPE)
            .with_columns(&["id"])
            .with_filter(export_filter(field, "x"));
        let mut samples: Vec<Duration> = (0..SAMPLES)
            .map(|_| {
                let start = Instant::now();
                let _ = plan_with(&core, who, &spec);
                start.elapsed()
            })
            .collect();
        samples.sort();
        samples[SAMPLES / 2]
    };
    let protected = time("card_number");
    let missing = time("no_such_field");
    let (lo, hi) = if protected < missing {
        (protected, missing)
    } else {
        (missing, protected)
    };
    assert!(
        hi - lo < Duration::from_millis(2) || hi < lo * 8,
        "refusal medians differ too much: protected {protected:?} vs missing {missing:?}"
    );
}

// ----------------------------------------------------------------- reports

fn filter_report(field: &str) -> String {
    report_toml(
        &["number"],
        &format!(
            "[[parameters]]\nname = \"p\"\nlabel = \"P\"\ntype = \"text\"\nrequired = true\n\n[filters]\nfield = \"{field}\"\nop = \"eq\"\nparam = \"p\"\n"
        ),
    )
}

#[tokio::test]
async fn a_report_filter_sort_group_or_aggregate_on_a_protected_field_is_refused() {
    let w = world().await;
    let secret = corpus().get("card").unwrap().value().to_owned();
    let args = || ReportArgs::from_json(&serde_json::json!({ "p": secret })).unwrap();
    let sort = report_toml(&["number"], "[[order_by]]\nfield = \"card_number\"\n");
    let grouped = "name = \"g\"\nlabel = \"G\"\nbase = \"sales_invoice\"\nroles = [\"Analyst\"]\ngroup_by = [\"card_number\"]\n\n[[columns]]\nfield = \"card_number\"\n\n[[columns]]\nfield = \"bonus\"\naggregate = \"sum\"\n\n[[order_by]]\nfield = \"card_number\"\n".to_owned();
    let summed = "name = \"s\"\nlabel = \"S\"\nbase = \"sales_invoice\"\nroles = [\"Analyst\"]\ngroup_by = [\"number\"]\n\n[[columns]]\nfield = \"number\"\n\n[[columns]]\nfield = \"bonus\"\naggregate = \"sum\"\n\n[[order_by]]\nfield = \"number\"\n".to_owned();

    for who in unauthorised() {
        if who == "nobody" {
            continue; // no read right on the DocType: an empty result, not an error (no existence signal)
        }
        for (label, toml, run_args) in [
            ("filter", filter_report("card_number"), args()),
            ("sort", sort.clone(), ReportArgs::empty()),
            ("group", grouped.clone(), ReportArgs::empty()),
        ] {
            let err = w.run(who, &toml, run_args).await.expect_err(label);
            assert_eq!(err.code(), "permission.field_denied", "{who} {label}");
            assert_no_leak(&corpus(), &[Output::Text(format!("{} {err}", err.code()))]);
        }
        // An aggregate over `bonus` (finance) is refused for a principal who cannot read it
        // unmasked; one who can (no_cards holds finance) gets the sum.
        let finance = may_read(who, "finance");
        let r = w.run(who, &summed, ReportArgs::empty()).await;
        assert_eq!(r.is_ok(), finance, "{who}: aggregate over a finance field");
    }
}

#[tokio::test]
async fn the_same_report_query_is_allowed_for_a_reader_of_the_field() {
    let w = world().await;
    let secret = corpus().get("card").unwrap().value().to_owned();
    let out = w
        .run(
            "analyst",
            &filter_report("card_number"),
            ReportArgs::from_json(&serde_json::json!({ "p": secret })).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        out.result.rows.len(),
        3,
        "the filter matches the three seeded invoices"
    );
}

#[test]
fn export_format_is_not_part_of_the_decision() {
    // The same refusal on CSV and NDJSON.
    let core = core();
    for format in [Format::Csv, Format::Ndjson] {
        let spec = ExportSpec::new(DOCTYPE, format)
            .with_columns(&["id"])
            .with_filter(export_filter("card_number", "x"));
        assert!(plan_with(&core, "masked_reader", &spec).is_err());
    }
}
