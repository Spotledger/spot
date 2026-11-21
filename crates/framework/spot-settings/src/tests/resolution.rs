//! Resolution: the narrowest set value wins, and equals a naive model on random input.

use proptest::prelude::*;
use spot_mgmt_ops::ParamValue;

use crate::model::{EffectiveValue, Origin, Scope, ScopeRef, SecretValue, StoredValue};
use crate::resolution::{Chain, resolve};
use crate::store::{MemoryStore, ValueStore};
use crate::tests::{build, int_spec, text_spec};

fn chain_of(scopes: Vec<ScopeRef>) -> Chain {
    Chain::new(scopes).unwrap()
}

fn scope_ref(level: Scope, id: &str) -> ScopeRef {
    match level {
        Scope::Fleet => ScopeRef::Fleet,
        Scope::Group => ScopeRef::Group(id.to_owned()),
        Scope::Server => ScopeRef::Server(id.to_owned()),
        Scope::Tenant => ScopeRef::Tenant(id.to_owned()),
        Scope::Company => ScopeRef::Company(id.to_owned()),
        Scope::User => ScopeRef::User(id.to_owned()),
    }
}

#[test]
fn the_narrowest_set_value_wins_and_the_chain_explains_it() {
    let registry = build(vec![text_spec(
        "runtime.http.address",
        "0.0.0.0:8080",
        vec![Scope::Fleet, Scope::Group, Scope::Server],
    )]);
    let store = MemoryStore::new();
    store.put(
        &ScopeRef::Fleet,
        "runtime.http.address",
        StoredValue::Value(ParamValue::Text("fleet".to_owned())),
    );
    store.put(
        &ScopeRef::Server("srv-17".to_owned()),
        "runtime.http.address",
        StoredValue::Value(ParamValue::Text("server".to_owned())),
    );
    let chain = chain_of(vec![
        ScopeRef::Fleet,
        ScopeRef::Group("emea".to_owned()),
        ScopeRef::Server("srv-17".to_owned()),
    ]);

    let resolved = resolve("runtime.http.address", &chain, &store, &registry).unwrap();
    assert_eq!(
        resolved.value,
        EffectiveValue::Value(ParamValue::Text("server".to_owned()))
    );
    assert_eq!(
        resolved.origin,
        Origin::Set(ScopeRef::Server("srv-17".to_owned()))
    );
    assert_eq!(resolved.chain.len(), 3);
    assert_eq!(resolved.chain[1].stored, None);
    assert_eq!(
        resolved.chain[0].stored,
        Some(EffectiveValue::Value(ParamValue::Text("fleet".to_owned())))
    );
}

#[test]
fn an_unset_setting_resolves_to_its_default() {
    let registry = build(vec![int_spec(
        "runtime.jobs.workers",
        1,
        64,
        4,
        vec![Scope::Fleet, Scope::Server],
    )]);
    let store = MemoryStore::new();
    let chain = chain_of(vec![ScopeRef::Fleet, ScopeRef::Server("srv".to_owned())]);
    let resolved = resolve("runtime.jobs.workers", &chain, &store, &registry).unwrap();
    assert_eq!(resolved.origin, Origin::Default);
    assert_eq!(resolved.value, EffectiveValue::Value(ParamValue::Int(4)));
}

#[test]
fn a_value_at_a_disallowed_scope_is_ignored() {
    let registry = build(vec![text_spec(
        "runtime.http.address",
        "default",
        vec![Scope::Fleet, Scope::Server],
    )]);
    let store = MemoryStore::new();
    store.put(
        &ScopeRef::Group("emea".to_owned()),
        "runtime.http.address",
        StoredValue::Value(ParamValue::Text("group".to_owned())),
    );
    let chain = chain_of(vec![
        ScopeRef::Fleet,
        ScopeRef::Group("emea".to_owned()),
        ScopeRef::Server("srv".to_owned()),
    ]);
    let resolved = resolve("runtime.http.address", &chain, &store, &registry).unwrap();
    assert_eq!(resolved.origin, Origin::Default);
    assert_eq!(resolved.chain[1].stored, None);
}

#[test]
fn a_secret_resolves_to_redacted_and_never_leaks() {
    const SECRET: &str = "s3cr3t-do-not-leak";
    let mut secret = text_spec(
        "runtime.smtp.password",
        "",
        vec![Scope::Fleet, Scope::Server],
    );
    secret.secret = true;
    secret.default = None;
    let registry = build(vec![secret]);

    let store = MemoryStore::new();
    store.put(
        &ScopeRef::Server("srv".to_owned()),
        "runtime.smtp.password",
        StoredValue::Secret(SecretValue::new(SECRET)),
    );
    let chain = chain_of(vec![ScopeRef::Fleet, ScopeRef::Server("srv".to_owned())]);
    let resolved = resolve("runtime.smtp.password", &chain, &store, &registry).unwrap();
    assert_eq!(resolved.value, EffectiveValue::Redacted);
    assert_eq!(
        resolved.origin,
        Origin::Set(ScopeRef::Server("srv".to_owned()))
    );
    assert_eq!(resolved.chain[1].stored, Some(EffectiveValue::Redacted));

    let debug = format!("{resolved:?}");
    assert!(!debug.contains(SECRET), "secret leaked in Debug: {debug}");
    let json = serde_json::to_string(&resolved).unwrap();
    assert!(!json.contains(SECRET), "secret leaked in JSON: {json}");
}

#[test]
fn an_unregistered_key_is_an_unknown_setting() {
    let registry = build(vec![int_spec(
        "runtime.jobs.workers",
        1,
        64,
        4,
        vec![Scope::Fleet],
    )]);
    let store = MemoryStore::new();
    let chain = chain_of(vec![ScopeRef::Fleet]);
    let report = resolve("runtime.nope", &chain, &store, &registry).unwrap_err();
    assert!(report.has_code("settings.unknown_setting"));
}

#[test]
fn a_chain_must_be_broad_to_narrow() {
    let report = Chain::new(vec![ScopeRef::Server("srv".to_owned()), ScopeRef::Fleet]).unwrap_err();
    assert!(report.has_code("settings.scopes_order"));
}

// The naive model this property is checked against: walk broad to narrow, keep the last
// scope that has a value; if none, use the default.
proptest! {
    #[test]
    fn resolve_equals_the_naive_model(
        levels in proptest::collection::btree_set(0usize..6, 1..=6),
        data in proptest::collection::vec((any::<bool>(), 0i64..100), 6),
        default in 0i64..100,
    ) {
        let levels: Vec<usize> = levels.into_iter().collect();
        let registry = build(vec![int_spec(
            "runtime.jobs.workers",
            0,
            100,
            default,
            Scope::ALL.to_vec(),
        )]);
        let store = MemoryStore::new();

        let mut scopes = Vec::new();
        let mut expected_value: Option<i64> = None;
        let mut expected_scope: Option<ScopeRef> = None;
        for (index, level) in levels.iter().enumerate() {
            let (present, value) = data[index];
            let scope = scope_ref(Scope::ALL[*level], &format!("id{index}"));
            if present {
                store.put(&scope, "runtime.jobs.workers", StoredValue::Value(ParamValue::Int(value)));
                expected_value = Some(value);
                expected_scope = Some(scope.clone());
            }
            scopes.push(scope);
        }

        let chain = Chain::new(scopes).unwrap();
        let resolved = resolve("runtime.jobs.workers", &chain, &store, &registry).unwrap();

        match expected_scope {
            Some(scope) => {
                prop_assert_eq!(resolved.origin, Origin::Set(scope));
                prop_assert_eq!(resolved.value, EffectiveValue::Value(ParamValue::Int(expected_value.unwrap())));
            }
            None => {
                prop_assert_eq!(resolved.origin, Origin::Default);
                prop_assert_eq!(resolved.value, EffectiveValue::Value(ParamValue::Int(default)));
            }
        }
        prop_assert_eq!(resolved.chain.len(), levels.len());
    }
}
