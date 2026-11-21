//! Registry builder validation: every problem is reported, with its stable code.

use spot_mgmt_ops::{ParamType, ParamValue};

use crate::builder::RegistryBuilder;
use crate::model::{DependsOn, Deprecated, Scope};
use crate::tests::{build, codes, int_spec, spec, text_spec};

fn low_scopes() -> Vec<Scope> {
    vec![Scope::Fleet, Scope::Server]
}

#[test]
fn a_valid_registry_builds_in_key_order() {
    let registry = build(vec![
        int_spec("runtime.jobs.workers", 1, 64, 4, low_scopes()),
        text_spec("runtime.http.address", "0.0.0.0:8080", low_scopes()),
    ]);
    assert_eq!(registry.len(), 2);
    let keys: Vec<&str> = registry.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["runtime.http.address", "runtime.jobs.workers"]);
    assert!(registry.contains_key("runtime.http.address"));
    assert_eq!(registry.specs_in_area("runtime").len(), 2);
    assert!(registry.specs_in_area("os").is_empty());
}

#[test]
fn duplicate_keys_are_refused() {
    let report = RegistryBuilder::new()
        .register_all(vec![
            text_spec("runtime.http.address", "a", low_scopes()),
            text_spec("runtime.http.address", "b", low_scopes()),
        ])
        .build()
        .expect_err("duplicate keys must fail");
    assert!(report.has_code("settings.duplicate_key"));
}

#[test]
fn an_invalid_default_is_refused() {
    let mut bad = int_spec("runtime.jobs.workers", 1, 8, 4, low_scopes());
    // Force an out-of-range default past the helper.
    bad.default = Some(ParamValue::Int(99));
    let report = RegistryBuilder::new()
        .push_spec(bad)
        .build()
        .expect_err("out-of-range default must fail");
    assert!(report.has_code("settings.default_invalid"));
}

#[test]
fn a_missing_default_is_refused_for_a_non_secret() {
    let mut bad = text_spec("runtime.http.address", "x", low_scopes());
    bad.default = None;
    let report = RegistryBuilder::new()
        .push_spec(bad)
        .build()
        .expect_err("missing default must fail");
    assert!(report.has_code("settings.default_missing"));
}

#[test]
fn a_secret_default_is_refused_and_a_secret_without_one_is_allowed() {
    let mut secret = text_spec("runtime.smtp.password", "hunter2", low_scopes());
    secret.secret = true;
    let report = RegistryBuilder::new()
        .push_spec(secret)
        .build()
        .expect_err("a secret default must fail");
    assert!(report.has_code("settings.secret_with_default"));

    let mut ok = text_spec("runtime.smtp.password", "ignored", low_scopes());
    ok.secret = true;
    ok.default = None;
    build(vec![ok]);
}

#[test]
fn empty_and_misordered_scopes_are_refused() {
    let empty = spec(
        "runtime.http.address",
        ParamType::Bool,
        Some(ParamValue::Bool(true)),
        Vec::new(),
    );
    let report = RegistryBuilder::new()
        .push_spec(empty)
        .build()
        .expect_err("empty scopes must fail");
    assert!(report.has_code("settings.scopes_empty"));

    let misordered = spec(
        "runtime.http.address",
        ParamType::Bool,
        Some(ParamValue::Bool(true)),
        vec![Scope::Server, Scope::Fleet],
    );
    let report = RegistryBuilder::new()
        .push_spec(misordered)
        .build()
        .expect_err("misordered scopes must fail");
    assert!(report.has_code("settings.scopes_order"));
}

#[test]
fn unknown_and_cyclic_depends_on_are_refused() {
    let mut unknown = text_spec("runtime.http.address", "x", low_scopes());
    unknown.depends_on = Some(DependsOn {
        key: "runtime.nope".to_owned(),
        equals: ParamValue::Bool(true),
    });
    let report = RegistryBuilder::new()
        .push_spec(unknown)
        .build()
        .expect_err("unknown depends_on must fail");
    assert!(report.has_code("settings.depends_on_unknown"));

    let mut left = text_spec("runtime.left", "x", low_scopes());
    left.depends_on = Some(DependsOn {
        key: "runtime.right".to_owned(),
        equals: ParamValue::Text("x".to_owned()),
    });
    let mut right = text_spec("runtime.right", "x", low_scopes());
    right.depends_on = Some(DependsOn {
        key: "runtime.left".to_owned(),
        equals: ParamValue::Text("x".to_owned()),
    });
    let report = RegistryBuilder::new()
        .register_all(vec![left, right])
        .build()
        .expect_err("cyclic depends_on must fail");
    assert!(report.has_code("settings.depends_on_cycle"));
}

#[test]
fn a_deprecation_pointing_nowhere_is_refused() {
    let mut spec = text_spec("runtime.old", "x", low_scopes());
    spec.deprecated = Some(Deprecated {
        since: "0.2.0".to_owned(),
        use_instead: "runtime.new".to_owned(),
    });
    let report = RegistryBuilder::new()
        .push_spec(spec)
        .build()
        .expect_err("unknown deprecation target must fail");
    assert!(report.has_code("settings.deprecated_unknown"));
}

#[test]
fn a_valid_deprecation_and_dependency_build() {
    let old = text_spec("runtime.new", "x", low_scopes());
    let mut legacy = text_spec("runtime.old", "x", low_scopes());
    legacy.deprecated = Some(Deprecated {
        since: "0.2.0".to_owned(),
        use_instead: "runtime.new".to_owned(),
    });
    let mut dependent = text_spec("runtime.child", "x", low_scopes());
    dependent.depends_on = Some(DependsOn {
        key: "runtime.new".to_owned(),
        equals: ParamValue::Text("x".to_owned()),
    });
    build(vec![old, legacy, dependent]);
}

#[test]
fn malformed_keys_are_refused() {
    let cases = [
        ("single", "too few segments"),
        ("Runtime.http", "upper-case area"),
        ("runtime..address", "empty segment"),
        ("runtime.9lives", "segment starts with a digit"),
        ("runtime.http-address", "hyphen is not allowed"),
    ];
    for (key, why) in cases {
        let bad = spec(
            key,
            ParamType::Bool,
            Some(ParamValue::Bool(true)),
            low_scopes(),
        );
        let report = RegistryBuilder::new()
            .push_spec(bad)
            .build()
            .expect_err(why);
        assert!(report.has_code("settings.key_format"), "{key}: {why}");
    }
}

#[test]
fn every_problem_is_reported_at_once() {
    let duplicate_a = text_spec("runtime.dup", "a", low_scopes());
    let duplicate_b = text_spec("runtime.dup", "b", low_scopes());
    let empty = spec(
        "runtime.empty",
        ParamType::Bool,
        Some(ParamValue::Bool(true)),
        Vec::new(),
    );
    let report = RegistryBuilder::new()
        .register_all(vec![duplicate_a, duplicate_b, empty])
        .build()
        .expect_err("several problems must fail");
    let found = codes(&report);
    assert!(found.contains(&"settings.duplicate_key"), "{found:?}");
    assert!(found.contains(&"settings.scopes_empty"), "{found:?}");
}
