//! Differs-from-default and the effective hash.

use spot_mgmt_ops::ParamValue;

use crate::diff::{differs, effective_hash};
use crate::model::{EffectiveValue, Origin, Scope, ScopeRef, StoredValue};
use crate::resolution::Chain;
use crate::store::{MemoryStore, ValueStore};
use crate::tests::{build, int_spec, text_spec};

fn chain() -> Chain {
    Chain::new(vec![ScopeRef::Fleet, ScopeRef::Server("srv-17".to_owned())]).unwrap()
}

#[test]
fn setting_a_value_equal_to_the_default_is_not_a_difference() {
    let registry = build(vec![int_spec(
        "runtime.jobs.workers",
        1,
        64,
        4,
        vec![Scope::Fleet, Scope::Server],
    )]);
    let store = MemoryStore::new();
    store.put(
        &ScopeRef::Server("srv-17".to_owned()),
        "runtime.jobs.workers",
        StoredValue::Value(ParamValue::Int(4)),
    );
    // Origin is Set, but the value equals the default, so it does not differ.
    let resolved =
        crate::resolution::resolve("runtime.jobs.workers", &chain(), &store, &registry).unwrap();
    assert!(matches!(resolved.origin, Origin::Set(_)));
    assert!(differs(&chain(), &store, &registry, None).is_empty());
}

#[test]
fn a_different_value_is_reported_with_its_scope() {
    let registry = build(vec![text_spec(
        "runtime.http.address",
        "0.0.0.0:8080",
        vec![Scope::Fleet, Scope::Server],
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

    let all = differs(&chain(), &store, &registry, None);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].key, "runtime.http.address");
    assert_eq!(
        all[0].value,
        EffectiveValue::Value(ParamValue::Text("server".to_owned()))
    );

    let at_server = differs(
        &chain(),
        &store,
        &registry,
        Some(&ScopeRef::Server("srv-17".to_owned())),
    );
    assert_eq!(at_server.len(), 1);
    let at_fleet = differs(&chain(), &store, &registry, Some(&ScopeRef::Fleet));
    assert!(at_fleet.is_empty(), "the fleet value is overridden");
}

#[test]
fn a_set_secret_differs_and_reads_redacted() {
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
        &ScopeRef::Server("srv-17".to_owned()),
        "runtime.smtp.password",
        StoredValue::Secret(crate::model::SecretValue::new("hidden")),
    );
    let differences = differs(&chain(), &store, &registry, None);
    assert_eq!(differences.len(), 1);
    assert_eq!(differences[0].value, EffectiveValue::Redacted);
    assert_eq!(differences[0].default, None);
}

#[test]
fn the_effective_hash_tracks_set_values_only() {
    let registry = build(vec![
        int_spec(
            "runtime.jobs.workers",
            1,
            64,
            4,
            vec![Scope::Fleet, Scope::Server],
        ),
        text_spec(
            "runtime.http.address",
            "default",
            vec![Scope::Fleet, Scope::Server],
        ),
    ]);
    let empty = MemoryStore::new();
    let baseline = effective_hash(&chain(), &empty, &registry);

    let one = MemoryStore::new();
    one.put(
        &ScopeRef::Fleet,
        "runtime.jobs.workers",
        StoredValue::Value(ParamValue::Int(8)),
    );
    let with_one = effective_hash(&chain(), &one, &registry);
    assert_ne!(baseline, with_one);

    // A different value changes the hash.
    let two = MemoryStore::new();
    two.put(
        &ScopeRef::Fleet,
        "runtime.jobs.workers",
        StoredValue::Value(ParamValue::Int(9)),
    );
    assert_ne!(with_one, effective_hash(&chain(), &two, &registry));

    // The same value in a fresh store hashes the same.
    let three = MemoryStore::new();
    three.put(
        &ScopeRef::Fleet,
        "runtime.jobs.workers",
        StoredValue::Value(ParamValue::Int(8)),
    );
    assert_eq!(with_one, effective_hash(&chain(), &three, &registry));
}

#[test]
fn the_effective_hash_never_contains_a_secret() {
    // The hash is a digest, so this asserts stability across secret text, not containment.
    let mut secret = text_spec("runtime.smtp.password", "", vec![Scope::Fleet]);
    secret.secret = true;
    secret.default = None;
    let registry = build(vec![secret]);
    let store = MemoryStore::new();
    store.put(
        &ScopeRef::Fleet,
        "runtime.smtp.password",
        StoredValue::Secret(crate::model::SecretValue::new("first")),
    );
    let first = effective_hash(&chain(), &store, &registry);
    let store = MemoryStore::new();
    store.put(
        &ScopeRef::Fleet,
        "runtime.smtp.password",
        StoredValue::Secret(crate::model::SecretValue::new("second")),
    );
    let second = effective_hash(&chain(), &store, &registry);
    assert_eq!(first, second);
}
