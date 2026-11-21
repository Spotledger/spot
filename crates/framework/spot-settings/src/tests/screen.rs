//! The generated screen model: golden fixture, control choice and row flags.

use spot_mgmt_ops::{ParamType, ParamValue};

use crate::catalogue::load_seed_catalogue;
use crate::model::{EffectiveValue, Scope, ScopeRef, SecretValue, StoredValue};
use crate::resolution::Chain;
use crate::screen::{Control, Row, screen_for};
use crate::store::{MemoryStore, ValueStore};
use crate::tests::{build, spec, text_spec};

fn seed_registry() -> crate::builder::SettingsRegistry {
    build(load_seed_catalogue().unwrap())
}

fn context_chain() -> Chain {
    Chain::new(vec![
        ScopeRef::Fleet,
        ScopeRef::Server("srv-17".to_owned()),
        ScopeRef::Tenant("acme".to_owned()),
        ScopeRef::Company("acme/C1".to_owned()),
    ])
    .unwrap()
}

fn context_store() -> MemoryStore {
    let store = MemoryStore::new();
    store.put(
        &ScopeRef::Server("srv-17".to_owned()),
        "runtime.http.listen_address",
        StoredValue::Value(ParamValue::Text("127.0.0.1:9090".to_owned())),
    );
    store.put(
        &ScopeRef::Tenant("acme".to_owned()),
        "runtime.session.idle_timeout",
        StoredValue::Value(ParamValue::Duration(900)),
    );
    store.put(
        &ScopeRef::Fleet,
        "runtime.smtp.password",
        StoredValue::Secret(SecretValue::new("golden-secret")),
    );
    store
}

fn rows_by_key(model: &crate::screen::ScreenModel) -> std::collections::BTreeMap<String, Row> {
    let mut rows = std::collections::BTreeMap::new();
    for group in &model.screen.groups {
        for row in &group.settings {
            rows.insert(row.key.clone(), row.clone());
        }
    }
    rows
}

#[test]
fn the_seed_screen_matches_the_golden_json() {
    let registry = seed_registry();
    let chain = context_chain();
    let store = context_store();
    let model = screen_for(
        "runtime",
        &registry,
        &chain,
        &store,
        Some(&ScopeRef::Server("srv-17".to_owned())),
    );
    let actual = serde_json::to_string_pretty(&model).unwrap();
    let expected = include_str!("../../tests/fixtures/screen_golden.json");
    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "the generated screen drifted from the golden fixture"
    );
}

#[test]
fn registering_one_new_spec_adds_exactly_one_row() {
    let chain = context_chain();
    let store = context_store();
    let editing = ScopeRef::Server("srv-17".to_owned());

    let before = screen_for("runtime", &seed_registry(), &chain, &store, Some(&editing));
    let before = rows_by_key(&before);

    let mut metrics = spec(
        "runtime.metrics.enabled",
        ParamType::Bool,
        Some(ParamValue::Bool(true)),
        vec![Scope::Fleet, Scope::Server],
    );
    metrics.title = "Metrics Enabled".to_owned();
    let registry = build(
        load_seed_catalogue()
            .unwrap()
            .into_iter()
            .chain(std::iter::once(metrics)),
    );

    let after = rows_by_key(&screen_for(
        "runtime",
        &registry,
        &chain,
        &store,
        Some(&editing),
    ));

    // The new row appears with the right control and value, with no other code change.
    let new_row = after
        .get("runtime.metrics.enabled")
        .expect("new row present");
    assert_eq!(new_row.control, Control::Toggle);
    assert_eq!(
        new_row.effective,
        EffectiveValue::Value(ParamValue::Bool(true))
    );
    assert!(!new_row.differs);
    assert!(new_row.can_edit);

    for (key, row) in &before {
        assert_eq!(after.get(key), Some(row), "row {key} changed unexpectedly");
    }
}

#[test]
fn every_value_type_maps_to_a_control() {
    assert_eq!(Control::from_param_type(&ParamType::Bool), Control::Toggle);
    assert_eq!(
        Control::from_param_type(&ParamType::Int { min: 1, max: 9 }),
        Control::Number {
            min: 1,
            max: 9,
            step: 1
        }
    );
    assert_eq!(
        Control::from_param_type(&ParamType::Text {
            max_len: 5,
            pattern: None
        }),
        Control::Text {
            max_len: 5,
            pattern: None
        }
    );
    assert_eq!(
        Control::from_param_type(&ParamType::Enum(vec!["a".to_owned()])),
        Control::Choice {
            options: vec!["a".to_owned()]
        }
    );
    assert_eq!(
        Control::from_param_type(&ParamType::Duration),
        Control::Duration
    );
    assert_eq!(
        Control::from_param_type(&ParamType::List(Box::new(ParamType::Bool))),
        Control::ListEditor {
            item: Box::new(Control::Toggle)
        }
    );
}

#[test]
fn secret_and_read_only_rows_get_their_controls_and_hide_the_secret() {
    let mut secret = text_spec("runtime.smtp.password", "", vec![Scope::Fleet]);
    secret.secret = true;
    secret.default = None;
    let mut readonly = spec(
        "os.kernel.version",
        ParamType::Text {
            max_len: 32,
            pattern: None,
        },
        Some(ParamValue::Text("unknown".to_owned())),
        vec![Scope::Fleet],
    );
    readonly.read_only = true;
    let registry = build(vec![secret, readonly]);

    let chain = Chain::new(vec![ScopeRef::Fleet]).unwrap();
    let store = MemoryStore::new();
    store.put(
        &ScopeRef::Fleet,
        "runtime.smtp.password",
        StoredValue::Secret(SecretValue::new("leak-me-not")),
    );
    let model = screen_for("runtime", &registry, &chain, &store, None);
    let rows = rows_by_key(&model);
    assert_eq!(rows["runtime.smtp.password"].control, Control::SecretInput);
    assert_eq!(
        rows["runtime.smtp.password"].effective,
        EffectiveValue::Redacted
    );

    let model = screen_for("os", &registry, &chain, &store, None);
    let rows = rows_by_key(&model);
    assert_eq!(rows["os.kernel.version"].control, Control::ReadOnlyValue);
    assert!(!rows["os.kernel.version"].can_edit);
}

#[test]
fn visibility_follows_depends_on() {
    let parent = spec(
        "runtime.feature.enabled",
        ParamType::Bool,
        Some(ParamValue::Bool(false)),
        vec![Scope::Fleet, Scope::Server],
    );
    let mut child = text_spec(
        "runtime.feature.endpoint",
        "http://x",
        vec![Scope::Fleet, Scope::Server],
    );
    child.depends_on = Some(crate::model::DependsOn {
        key: "runtime.feature.enabled".to_owned(),
        equals: ParamValue::Bool(true),
    });
    let registry = build(vec![parent, child]);
    let chain = Chain::new(vec![ScopeRef::Fleet]).unwrap();

    let store = MemoryStore::new();
    let hidden = screen_for("runtime", &registry, &chain, &store, None);
    assert!(!rows_by_key(&hidden)["runtime.feature.endpoint"].visible);

    store.put(
        &ScopeRef::Fleet,
        "runtime.feature.enabled",
        StoredValue::Value(ParamValue::Bool(true)),
    );
    let shown = screen_for("runtime", &registry, &chain, &store, None);
    assert!(rows_by_key(&shown)["runtime.feature.endpoint"].visible);
}

#[test]
fn can_edit_and_deprecation_flags_are_set() {
    let old = text_spec("runtime.new", "x", vec![Scope::Fleet]);
    let mut legacy = text_spec("runtime.old", "x", vec![Scope::Fleet, Scope::Server]);
    legacy.deprecated = Some(crate::model::Deprecated {
        since: "0.2.0".to_owned(),
        use_instead: "runtime.new".to_owned(),
    });
    let registry = build(vec![old, legacy]);
    let chain = Chain::new(vec![ScopeRef::Fleet, ScopeRef::Server("srv".to_owned())]).unwrap();
    let store = MemoryStore::new();

    let rows = rows_by_key(&screen_for(
        "runtime",
        &registry,
        &chain,
        &store,
        Some(&ScopeRef::Server("srv".to_owned())),
    ));
    assert!(rows["runtime.old"].can_edit);
    assert!(rows["runtime.old"].deprecated);
    assert!(
        rows["runtime.old"]
            .deprecated_message
            .as_deref()
            .unwrap()
            .contains("runtime.new")
    );
    // The fleet-only setting cannot be edited at the server scope.
    assert!(!rows["runtime.new"].can_edit);
}

#[test]
fn an_exact_key_selects_a_single_row_and_hints_order_the_group() {
    let registry = seed_registry();
    let chain = context_chain();
    let store = context_store();

    let single = screen_for(
        "runtime.http.listen_address",
        &registry,
        &chain,
        &store,
        None,
    );
    assert_eq!(single.screen.groups.len(), 1);
    assert_eq!(single.screen.groups[0].settings.len(), 1);

    // The Email group comes from the explicit hint and orders host (1) before password (2).
    let runtime = screen_for("runtime", &registry, &chain, &store, None);
    let email = runtime
        .screen
        .groups
        .iter()
        .find(|group| group.title == "Email")
        .expect("the hint groups smtp settings under Email");
    let keys: Vec<&str> = email.settings.iter().map(|row| row.key.as_str()).collect();
    assert_eq!(keys, vec!["runtime.smtp.host", "runtime.smtp.password"]);
}

#[test]
fn scope_chain_is_reported() {
    let registry = seed_registry();
    let chain = context_chain();
    let store = context_store();
    let model = screen_for("runtime", &registry, &chain, &store, None);
    assert_eq!(model.area_or_path, "runtime");
    assert_eq!(model.chain, chain.scopes().to_vec());
}
