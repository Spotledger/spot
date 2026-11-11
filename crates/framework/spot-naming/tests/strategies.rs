//! Naming strategies: golden names, corrupt inputs, the overflow rule and determinism.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_naming::{FiscalYearStart, FixedIdSource, GapPolicy, NamingError, StaticPlans, Strategy};
use spot_plan::{NamingPart, NamingPlan};
use spot_storage_contract::RecordId;
use time::Month;

async fn named(
    doctype: &str,
    doc: &spot_app_api::Document,
    ctx: &spot_naming::NamingContext,
) -> String {
    let backend = backend();
    let mut tx = begin(&backend).await;
    namer()
        .name_for(&name(doctype), doc, ctx, GapPolicy::Tolerant, &mut tx)
        .await
        .map(|n| n.to_string())
        .unwrap()
}

#[tokio::test]
async fn every_strategy_is_detected() {
    let namer = namer();
    assert_eq!(namer.strategy(SERIES_DOCTYPE).unwrap(), Strategy::Series);
    assert_eq!(
        namer.strategy("by_field").unwrap(),
        Strategy::Field("code".to_owned())
    );
    assert_eq!(namer.strategy("by_hash").unwrap(), Strategy::Hash);
    assert_eq!(namer.strategy("by_prompt").unwrap(), Strategy::Prompt);
}

#[tokio::test]
async fn an_unknown_doctype_has_no_strategy() {
    let err = namer().strategy("nowhere").unwrap_err();
    assert_eq!(err.code(), "naming.unknown_doctype");
}

#[tokio::test]
async fn a_series_name_is_prefix_date_and_padded_counter() {
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(5, 30));
    let doc = doc(SERIES_DOCTYPE);
    assert_eq!(named(SERIES_DOCTYPE, &doc, &ctx).await, "INV-2026-00001");
}

#[tokio::test]
async fn the_documented_pattern_renders_its_compiled_parts() {
    // `INV-.YYYY.-.#####` compiles to Literal("INV-"), Year(4), Literal(".-"), Counter(5): the
    // namer renders exactly that, without re-reading the pattern text.
    let plans = spot_naming::StaticPlans::new().with("invoice", plan("INV-.YYYY.-.#####"));
    let namer = spot_naming::Namer::new(std::sync::Arc::new(plans));
    let backend = backend();
    let mut tx = begin(&backend).await;
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0));
    let got = namer
        .name_for(
            &name("invoice"),
            &doc("invoice"),
            &ctx,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap();
    assert_eq!(got.as_str(), "INV-2026.-00001");
}

#[tokio::test]
async fn consecutive_names_walk_the_series() {
    let backend = backend();
    let namer = namer();
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0));
    let doctype = name(SERIES_DOCTYPE);
    let mut tx = begin(&backend).await;
    let first = namer
        .name_for(
            &doctype,
            &doc(SERIES_DOCTYPE),
            &ctx,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap();
    let second = namer
        .name_for(
            &doctype,
            &doc(SERIES_DOCTYPE),
            &ctx,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap();
    assert_eq!(first.as_str(), "INV-2026-00001");
    assert_eq!(second.as_str(), "INV-2026-00002");
}

#[tokio::test]
async fn a_counter_that_outgrows_its_width_widens() {
    let plans = spot_naming::StaticPlans::new().with("ticket", plan("N-.###"));
    let namer = spot_naming::Namer::new(std::sync::Arc::new(plans));
    let backend = backend();
    let doctype = name("ticket");
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0));
    let mut tx = begin(&backend).await;
    let key = namer.series_key(&doctype, &ctx).unwrap();
    namer.set_next(&key, 1000, &mut tx).await.unwrap();
    let got = namer
        .name_for(&doctype, &doc("ticket"), &ctx, GapPolicy::Tolerant, &mut tx)
        .await
        .unwrap();
    assert_eq!(got.as_str(), "N-1000", "the number is never truncated");
}

#[tokio::test]
async fn field_naming_reads_text_and_ints() {
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0));
    let mut text = doc("by_field");
    set_text(&mut text, "code", "ACME/7");
    assert_eq!(named("by_field", &text, &ctx).await, "ACME/7");

    let mut int = doc("by_field");
    int.set(name("code"), spot_storage_contract::Value::Int(7));
    assert_eq!(named("by_field", &int, &ctx).await, "7");
}

#[tokio::test]
async fn field_naming_without_the_field_is_reported() {
    let backend = backend();
    let mut tx = begin(&backend).await;
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0));
    let err = namer()
        .name_for(
            &name("by_field"),
            &doc("by_field"),
            &ctx,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "naming.missing_field");
}

#[tokio::test]
async fn hash_naming_is_deterministic_with_a_fixed_source() {
    let id = RecordId::new_v7();
    let namer = namer().with_id_source(std::sync::Arc::new(FixedIdSource::new(id)));
    let backend = backend();
    let mut tx = begin(&backend).await;
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0));
    let first = namer
        .name_for(
            &name("by_hash"),
            &doc("by_hash"),
            &ctx,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap();
    let second = namer
        .name_for(
            &name("by_hash"),
            &doc("by_hash"),
            &ctx,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first.as_str().len(), 26, "a Crockford base32 UUID");
    assert!(
        first
            .as_str()
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    );
}

#[tokio::test]
async fn prompt_naming_needs_a_supplied_name() {
    let backend = backend();
    let mut tx = begin(&backend).await;
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0));
    let err = namer()
        .name_for(
            &name("by_prompt"),
            &doc("by_prompt"),
            &ctx,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "naming.prompt_required");

    let supplied = ctx.with_supplied_name("MANUAL-1");
    let got = namer()
        .name_for(
            &name("by_prompt"),
            &doc("by_prompt"),
            &supplied,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap();
    assert_eq!(got.as_str(), "MANUAL-1");
}

#[tokio::test]
async fn a_supplied_name_must_be_legal() {
    let backend = backend();
    let mut tx = begin(&backend).await;
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0)).with_supplied_name(" padded ");
    let err = namer()
        .name_for(
            &name("by_prompt"),
            &doc("by_prompt"),
            &ctx,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, NamingError::InvalidName { .. }));
    assert_eq!(err.code(), "naming.invalid_name");
}

#[tokio::test]
async fn a_field_left_in_the_middle_of_a_pattern_is_invalid() {
    let plans = spot_naming::StaticPlans::new().with("mixed", plan("INV-field:code"));
    let namer = spot_naming::Namer::new(std::sync::Arc::new(plans));
    let err = namer.strategy("mixed").unwrap_err();
    assert_eq!(err.code(), "naming.pattern_invalid");
}

/// The compiled `.FY` part resolves through the injected fiscal-year resolver.
#[tokio::test]
async fn a_fiscal_year_part_uses_the_injected_resolver() {
    // Built directly because `spot-plan`'s parser folds `.FY` into literal text; see the module's
    // open questions. The parts are exactly what a `NamingPart::FiscalYear` would compile to.
    let plan = NamingPlan {
        parts: vec![
            NamingPart::Literal("FY-".to_owned()),
            NamingPart::Literal(".FY".to_owned()),
            NamingPart::Literal("-".to_owned()),
            NamingPart::Counter { width: 5 },
        ],
        field_refs: std::collections::BTreeSet::new(),
    };
    let backend = backend();
    let mut tx = begin(&backend).await;
    // 15 February 2026 is before a 1 April fiscal start, so the fiscal year is 2025.
    let ctx = ctx(at(2026, Month::February, 15, 9, 0), zone(0, 0));

    let april = spot_naming::Namer::new(std::sync::Arc::new(
        StaticPlans::new().with("fy", plan.clone()),
    ))
    .with_fiscal_year(std::sync::Arc::new(FiscalYearStart { month: 4, day: 1 }));
    let got = april
        .name_for(&name("fy"), &doc("fy"), &ctx, GapPolicy::Tolerant, &mut tx)
        .await
        .unwrap();
    assert_eq!(got.as_str(), "FY-2025-00001");

    let calendar =
        spot_naming::Namer::new(std::sync::Arc::new(StaticPlans::new().with("fy", plan)));
    let got = calendar
        .name_for(&name("fy"), &doc("fy"), &ctx, GapPolicy::Tolerant, &mut tx)
        .await
        .unwrap();
    assert_eq!(got.as_str(), "FY-2026-00001");
}
