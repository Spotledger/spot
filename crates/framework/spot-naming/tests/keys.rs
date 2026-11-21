//! Series keys: derivation, period resets, scope keys and over-long-key hashing.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_naming::{GapPolicy, StaticPlans};
use time::Month;

#[tokio::test]
async fn a_year_part_is_in_the_key_and_resets_each_year() {
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let backend = backend();
    let mut tx = begin(&backend).await;

    // 31 Dec 23:30 UTC is already 1 Jan in a zone ahead of UTC.
    let ahead = ctx(at(2025, Month::December, 31, 23, 30), zone(5, 30));
    // ... and still 31 Dec in a zone behind it.
    let behind = ctx(at(2025, Month::December, 31, 23, 30), zone(-8, 0));

    assert_eq!(
        namer.series_key(&doctype, &ahead).unwrap().as_str(),
        "INV-2026-"
    );
    assert_eq!(
        namer.series_key(&doctype, &behind).unwrap().as_str(),
        "INV-2025-"
    );

    let first_ahead = namer
        .name_for(
            &doctype,
            &doc(SERIES_DOCTYPE),
            &ahead,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap();
    let first_behind = namer
        .name_for(
            &doctype,
            &doc(SERIES_DOCTYPE),
            &behind,
            GapPolicy::Tolerant,
            &mut tx,
        )
        .await
        .unwrap();
    assert_eq!(first_ahead.as_str(), "INV-2026-00001");
    assert_eq!(
        first_behind.as_str(),
        "INV-2025-00001",
        "the 2025 series starts again at 1"
    );
}

#[tokio::test]
async fn a_pattern_without_a_date_never_resets() {
    let plans = StaticPlans::new().with("pos", plan("POS-.#####"));
    let namer = spot_naming::Namer::new(std::sync::Arc::new(plans));
    let doctype = name("pos");
    let in_2026 = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0));
    let in_2027 = ctx(at(2027, Month::June, 1, 12, 0), zone(0, 0));
    assert_eq!(
        namer.series_key(&doctype, &in_2026).unwrap().as_str(),
        "POS-"
    );
    assert_eq!(
        namer.series_key(&doctype, &in_2027).unwrap().as_str(),
        "POS-"
    );
}

#[tokio::test]
async fn the_business_date_beats_the_invocation_time() {
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0)).with_business_date(date(
        2027,
        Month::January,
        5,
    ));
    assert_eq!(
        namer.series_key(&doctype, &ctx).unwrap().as_str(),
        "INV-2027-"
    );
}

#[tokio::test]
async fn a_scope_key_makes_a_separate_series() {
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let scoped = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0)).with_scope_key("company7");
    assert_eq!(
        namer.series_key(&doctype, &scoped).unwrap().as_str(),
        "INV-2026-:company7"
    );
}

#[tokio::test]
async fn an_over_long_key_is_hashed_with_a_stable_prefix() {
    let pattern = format!("{}-.#####", "X".repeat(200));
    let plans = StaticPlans::new().with("long", plan(&pattern));
    let namer = spot_naming::Namer::new(std::sync::Arc::new(plans));
    let doctype = name("long");
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0));
    let key = namer.series_key(&doctype, &ctx).unwrap();
    assert!(key.as_str().starts_with("h:"), "{}", key.as_str());
    assert_eq!(key.as_str().len(), 34);
    // The same inputs always give the same key.
    assert_eq!(key, namer.series_key(&doctype, &ctx).unwrap());
}

#[tokio::test]
async fn the_same_inputs_always_give_the_same_key() {
    let namer = namer();
    let doctype = name(SERIES_DOCTYPE);
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(2, 0));
    assert_eq!(
        namer.series_key(&doctype, &ctx).unwrap(),
        namer.series_key(&doctype, &ctx).unwrap()
    );
}

#[tokio::test]
async fn a_non_series_doctype_has_no_series_key() {
    let err = namer().series_key(
        &name("by_hash"),
        &ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0)),
    );
    assert_eq!(err.unwrap_err().code(), "naming.pattern_invalid");
}
