//! The `scripting` app directory loads through `spot-meta` and `spot-permissions`, and nobody
//! but the two administrator roles holds any right on its two DocTypes (WP-EXT-06.T6).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use spot_meta::{Registry, load_dir};
use spot_permissions::{ModelTexts, load};

fn app_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../apps/scripting")
}

#[test]
fn the_doctypes_load_and_compile() {
    let definitions = load_dir(&app_dir().join("doctypes")).expect("the DocTypes parse");
    let names: Vec<&str> = definitions.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["api_script", "scheduled_script"]);
    Registry::new(definitions).expect("the definitions compile together");
}

#[test]
fn only_the_two_administrators_hold_any_right() {
    let grants = std::fs::read_to_string(app_dir().join("permissions/grants.toml"))
        .expect("the grants file exists");
    let model = load(&ModelTexts {
        grants,
        ..ModelTexts::default()
    })
    .expect("the permission model loads");

    assert_eq!(model.grants.len(), 4, "two roles on two DocTypes");
    for grant in &model.grants {
        assert!(
            ["Tenant Administrator", "System Manager"].contains(&grant.role.0.as_str()),
            "{} must not hold a right on {}",
            grant.role.0,
            grant.doctype
        );
        assert!(
            ["api_script", "scheduled_script"].contains(&grant.doctype.as_str()),
            "a grant outside the app's own DocTypes: {}",
            grant.doctype
        );
        assert!(
            !grant.if_owner,
            "the administrators' rights are unconditional"
        );
    }
    for doctype in ["api_script", "scheduled_script"] {
        for role in ["Tenant Administrator", "System Manager"] {
            assert!(
                model
                    .grants
                    .iter()
                    .any(|g| g.doctype == doctype && g.role.0 == role),
                "{role} has no grant on {doctype}"
            );
        }
    }
    assert!(model.field_grants.is_empty());
    assert!(model.shares.is_empty());
}

#[test]
fn the_app_manifest_names_the_app_and_declares_no_events() {
    let manifest = std::fs::read_to_string(app_dir().join("app.toml")).expect("app.toml exists");
    assert!(manifest.contains("name = \"scripting\""));
    assert!(!manifest.contains("[[events]]"));
    assert!(!manifest.contains("controller ="));
}
