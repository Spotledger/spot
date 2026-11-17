//! The services registry's names: what a `[services.<name>]` table may be called, and that the
//! legacy modules and the settings-only `idempotency` table are accepted (WP-PRD-01.T56).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_cli::config::Services;
use spot_cli::services::{REGISTRY, SETTINGS_ONLY, registered_names};

fn services(toml_text: &str) -> Services {
    toml::from_str(toml_text).expect("the services table parses")
}

#[test]
fn idempotency_is_a_known_settings_table() {
    let names = registered_names();
    assert!(names.contains(&"idempotency"));
    let parsed = services(
        "[idempotency]\nallow_memory_idempotency = true\n[idempotency.retention]\ndefault = \"24h\"\n",
    );
    let problems = parsed.validate();
    assert!(
        problems.iter().all(|p| !p.contains("idempotency")),
        "{problems:?}"
    );
}

#[test]
fn an_unknown_table_is_still_refused_by_name() {
    let parsed = services("[idempotencyy]\nx = 1\n");
    let problems = parsed.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("services.idempotencyy: unknown table")),
        "{problems:?}"
    );
}

#[test]
fn every_registry_module_name_is_listed_and_settings_only_names_are_not_modules() {
    let names = registered_names();
    for r in REGISTRY {
        assert!(names.contains(&r.name), "{} is not a known table", r.name);
    }
    for name in SETTINGS_ONLY {
        assert!(!REGISTRY.iter().any(|r| r.name == *name));
    }
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "a table name is listed twice");
}

#[test]
fn the_legacy_services_start_in_the_order_they_always_had() {
    let order_of = |name: &str| REGISTRY.iter().find(|r| r.name == name).map(|r| r.order);
    for (name, order) in [
        ("jobs", 600),
        ("mail", 700),
        ("realtime", 800),
        ("files", 900),
        ("print", 900),
        ("telemetry", 900),
    ] {
        assert_eq!(order_of(name), Some(order), "{name}");
    }
}
