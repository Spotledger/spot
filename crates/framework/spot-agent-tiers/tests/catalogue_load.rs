//! Loading the action catalogue: the happy path from the seed file, and every load error
//! (duplicate names, an effect's minimum tier, an empty name, broken TOML).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_agent_tiers::{ActionCatalogue, Tier, TierError};

use common::catalogue;

#[test]
fn the_seed_catalogue_loads_and_answers() {
    let cat = catalogue();
    assert_eq!(
        cat.len(),
        8,
        "the seed has eight actions over four tiers and two domains"
    );
    let spec = cat.get("install_app").expect("catalogued");
    assert_eq!(spec.tier, Tier::T3);
    assert_eq!(spec.domain, spot_agent_tiers::Domain::Support);
    assert_eq!(spec.effect, spot_agent_tiers::Effect::Structural);
    assert!(spec.is_change());
    assert!(
        !cat.get("explain_failed_job")
            .expect("catalogued")
            .is_change()
    );
    assert!(
        cat.get("nope").is_none(),
        "the unknown action is not catalogued"
    );
}

#[test]
fn duplicate_action_names_are_rejected() {
    let err = ActionCatalogue::from_toml(
        r#"
[[actions]]
name = "explode_the_pumpkin"
tier = "T1"
domain = "support"
effect = "reversible"

[[actions]]
name = "explode_the_pumpkin"
tier = "T1"
domain = "support"
effect = "reversible"
"#,
    )
    .expect_err("duplicates rejected");
    match err {
        TierError::DuplicateAction { name, .. } => assert_eq!(name, "explode_the_pumpkin"),
        other => panic!("wrong error: {other}"),
    }
}

#[test]
fn a_structural_effect_declared_as_t1_is_a_load_error() {
    let err = ActionCatalogue::from_toml(
        r#"
[[actions]]
name = "install_app"
tier = "T1"
domain = "support"
effect = "structural"
"#,
    )
    .expect_err("the effect's minimum tier is enforced");
    match err {
        TierError::EffectBelowFloor {
            declared,
            effect,
            minimum,
            ..
        } => {
            assert_eq!(declared, "T1");
            assert_eq!(effect, "structural");
            assert_eq!(minimum, "T3");
        }
        other => panic!("wrong error: {other}"),
    }
}

#[test]
fn every_effect_has_its_minimum_tier_enforced() {
    // read below T0 is impossible (T0 is the floor); each other effect one tier down is an
    // error:
    for (effect, declared) in [
        ("reversible", "T0"),
        ("data_repair", "T1"),
        ("structural", "T2"),
    ] {
        let text = format!(
            r#"
[[actions]]
name = "mystery_move"
tier = "{declared}"
domain = "support"
effect = "{effect}"
"#
        );
        let err = ActionCatalogue::from_toml(&text).expect_err("below the effect's floor");
        assert!(
            matches!(err, TierError::EffectBelowFloor { .. }),
            "wrong error for {effect}: {err}"
        );
    }

    // And each effect at exactly its minimum tier loads.
    for (effect, declared) in [
        ("read", "T0"),
        ("reversible", "T1"),
        ("data_repair", "T2"),
        ("structural", "T3"),
    ] {
        let text = format!(
            r#"
[[actions]]
name = "by_the_book"
tier = "{declared}"
domain = "support"
effect = "{effect}"
"#
        );
        ActionCatalogue::from_toml(&text).expect("at the minimum tier it loads");
    }
}

#[test]
fn an_empty_name_and_broken_toml_are_load_errors() {
    let err = ActionCatalogue::from_toml(
        r#"
[[actions]]
name = "  "
tier = "T0"
domain = "support"
effect = "read"
"#,
    )
    .expect_err("an empty name");
    assert!(
        matches!(err, TierError::ValueInvalid { .. }),
        "wrong error: {err}"
    );

    let err = ActionCatalogue::from_toml("not = toml at all [[[").expect_err("broken toml");
    assert!(matches!(err, TierError::Parse { .. }), "wrong error: {err}");

    // A key the catalogue does not know is an error, never ignored (house style).
    let err = ActionCatalogue::from_toml(
        r#"
[[actions]]
name = "explain_failed_job"
tier = "T0"
domain = "support"
effect = "read"
permlevel = 3
"#,
    )
    .expect_err("unknown keys are errors");
    assert!(matches!(err, TierError::Parse { .. }), "wrong error: {err}");
}

#[test]
fn an_empty_catalogue_is_legal_and_refuses_everything() {
    let cat = ActionCatalogue::new();
    assert!(cat.is_empty());
    assert_eq!(cat.len(), 0);
}
