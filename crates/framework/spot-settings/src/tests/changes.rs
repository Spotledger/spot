//! Change sets: validation, stable hashing, summarisation and application.

use std::sync::atomic::{AtomicUsize, Ordering};

use spot_mgmt_ops::{ParamType, ParamValue, Risk};

use crate::changes::{Change, ChangeSet, apply_changes, summarise, validate_changes};
use crate::model::{EffectiveValue, Restart, Scope, ScopeRef, StoredValue};
use crate::store::{AppliedWrite, MemoryStore, SettingWrite, ValueStore};
use crate::tests::{build, int_spec, spec, text_spec};

fn server(id: &str) -> ScopeRef {
    ScopeRef::Server(id.to_owned())
}

fn mixed_registry() -> crate::builder::SettingsRegistry {
    let mut http = text_spec(
        "runtime.http.address",
        "0.0.0.0:8080",
        vec![Scope::Fleet, Scope::Server],
    );
    http.risk = Risk::Medium;
    http.restart = Restart::Service {
        name: "http".to_owned(),
    };
    let mut readonly = spec(
        "runtime.readonly",
        ParamType::Bool,
        Some(ParamValue::Bool(false)),
        vec![Scope::Server],
    );
    readonly.read_only = true;
    let mut secret = text_spec(
        "runtime.smtp.password",
        "",
        vec![Scope::Fleet, Scope::Server],
    );
    secret.secret = true;
    secret.default = None;
    secret.risk = Risk::High;
    secret.restart = Restart::Service {
        name: "email".to_owned(),
    };
    let mut child = text_spec("runtime.child", "x", vec![Scope::Fleet, Scope::Server]);
    child.depends_on = Some(crate::model::DependsOn {
        key: "runtime.http.address".to_owned(),
        equals: ParamValue::Text("a".to_owned()),
    });
    build(vec![
        http,
        readonly,
        secret,
        child,
        int_spec(
            "runtime.jobs.workers",
            1,
            64,
            4,
            vec![Scope::Fleet, Scope::Server],
        ),
        spec(
            "runtime.logging.level",
            ParamType::Enum(vec!["info".to_owned(), "debug".to_owned()]),
            Some(ParamValue::Text("info".to_owned())),
            vec![Scope::Fleet, Scope::Server],
        ),
        spec(
            "runtime.short",
            ParamType::Text {
                max_len: 4,
                pattern: None,
            },
            Some(ParamValue::Text("ok".to_owned())),
            vec![Scope::Fleet],
        ),
    ])
}

#[test]
fn unknown_settings_read_only_and_scope_errors_are_reported() {
    let registry = mixed_registry();
    let changes = vec![
        Change::set(ScopeRef::Fleet, "runtime.nope", ParamValue::Int(1)),
        Change::set(server("srv"), "runtime.readonly", ParamValue::Bool(true)),
        Change::set(
            ScopeRef::Tenant("acme".to_owned()),
            "runtime.http.address",
            ParamValue::Text("x".to_owned()),
        ),
    ];
    let report = validate_changes(&changes, &registry).unwrap_err();
    assert!(report.has_code("settings.unknown_setting"));
    assert!(report.has_code("settings.read_only"));
    assert!(report.has_code("settings.scope_not_allowed"));
}

#[test]
fn value_failures_are_reported_all_at_once() {
    let registry = mixed_registry();
    let cases: Vec<Change> = vec![
        Change::set(
            ScopeRef::Fleet,
            "runtime.jobs.workers",
            ParamValue::Int(999),
        ),
        Change::set(
            ScopeRef::Fleet,
            "runtime.jobs.workers",
            ParamValue::Bool(true),
        ),
        Change::set(
            ScopeRef::Fleet,
            "runtime.logging.level",
            ParamValue::Text("nope".to_owned()),
        ),
        Change::set(
            ScopeRef::Fleet,
            "runtime.short",
            ParamValue::Text("too long".to_owned()),
        ),
    ];
    let report = validate_changes(&cases, &registry).unwrap_err();
    assert_eq!(
        report
            .errors()
            .iter()
            .filter(|error| error.code() == "settings.validation_failed")
            .count(),
        4,
        "{report}"
    );
}

#[test]
fn a_duplicate_change_is_refused() {
    let registry = mixed_registry();
    let changes = vec![
        Change::set(ScopeRef::Fleet, "runtime.jobs.workers", ParamValue::Int(2)),
        Change::set(ScopeRef::Fleet, "runtime.jobs.workers", ParamValue::Int(3)),
    ];
    let report = validate_changes(&changes, &registry).unwrap_err();
    assert!(report.has_code("settings.change_set_invalid"));
}

#[test]
fn the_change_set_hash_ignores_input_order() {
    let registry = mixed_registry();
    let first = ChangeSet::new()
        .push_change(Change::set(
            ScopeRef::Fleet,
            "runtime.jobs.workers",
            ParamValue::Int(2),
        ))
        .push_change(Change::set(
            server("srv"),
            "runtime.http.address",
            ParamValue::Text("x".to_owned()),
        ));
    let second = ChangeSet::new()
        .push_change(Change::set(
            server("srv"),
            "runtime.http.address",
            ParamValue::Text("x".to_owned()),
        ))
        .push_change(Change::set(
            ScopeRef::Fleet,
            "runtime.jobs.workers",
            ParamValue::Int(2),
        ));
    first.validate(&registry).unwrap();
    second.validate(&registry).unwrap();
    assert_eq!(first.hash(), second.hash());

    let different = ChangeSet::new().push_change(Change::set(
        ScopeRef::Fleet,
        "runtime.jobs.workers",
        ParamValue::Int(3),
    ));
    assert_ne!(first.hash(), different.hash());
}

#[test]
fn a_secret_never_appears_in_the_hash_or_debug() {
    const SECRET: &str = "do-not-hash-me";
    let set = ChangeSet::new().push_change(Change::set_secret(
        server("srv"),
        "runtime.smtp.password",
        SECRET,
    ));
    assert!(!set.hash().contains(SECRET));
    assert!(!format!("{set:?}").contains(SECRET));

    // Two different secret texts hash the same: the text is never part of the binding.
    let other = ChangeSet::new().push_change(Change::set_secret(
        server("srv"),
        "runtime.smtp.password",
        "a-different-secret",
    ));
    assert_eq!(set.hash(), other.hash());
}

#[test]
fn the_summary_names_risk_restart_and_dependents_without_a_secret() {
    const SECRET: &str = "hidden";
    let registry = mixed_registry();
    let set = ChangeSet::new()
        .push_change(Change::set_secret(
            server("srv"),
            "runtime.smtp.password",
            SECRET,
        ))
        .push_change(Change::set(
            ScopeRef::Fleet,
            "runtime.http.address",
            ParamValue::Text("a".to_owned()),
        ));
    let summary = summarise(&set, &registry);
    assert_eq!(summary.highest_risk, Risk::High);
    assert!(summary.restart_needed);
    assert!(summary.restarts.contains(&Restart::Service {
        name: "email".to_owned()
    }));
    assert!(
        summary
            .affected_via_depends_on
            .contains(&"runtime.child".to_owned())
    );
    assert!(!summary.summary.contains(SECRET));
    assert!(summary.summary.contains("secret"));
}

#[test]
fn applying_a_change_set_uses_the_batch_and_redacts_secrets() {
    const SECRET: &str = "applied-secret";
    let registry = mixed_registry();
    let store = SpyStore::default();
    let set = ChangeSet::new()
        .push_change(Change::set(
            ScopeRef::Fleet,
            "runtime.jobs.workers",
            ParamValue::Int(8),
        ))
        .push_change(Change::set_secret(
            server("srv"),
            "runtime.smtp.password",
            SECRET,
        ));

    set.validate(&registry).unwrap();
    let applied = apply_changes(&set, &registry, &store).unwrap();
    assert_eq!(store.applies.load(Ordering::SeqCst), 1, "one batch apply");
    assert_eq!(store.puts.load(Ordering::SeqCst), 0, "no per-item writes");

    let jobs = applied
        .iter()
        .find(|change| change.key == "runtime.jobs.workers")
        .unwrap();
    assert_eq!(jobs.before, None);
    assert_eq!(jobs.after, Some(EffectiveValue::Value(ParamValue::Int(8))));

    let secret = applied
        .iter()
        .find(|change| change.key == "runtime.smtp.password")
        .unwrap();
    assert_eq!(secret.after, Some(EffectiveValue::Redacted));
    assert!(!format!("{applied:?}").contains(SECRET));

    // The store does hold the secret, but nothing displays it.
    let stored = store
        .inner
        .get(&server("srv"), "runtime.smtp.password")
        .unwrap();
    match stored {
        StoredValue::Secret(value) => assert_eq!(value.expose(), SECRET),
        StoredValue::Value(_) => panic!("expected a stored secret"),
    }
}

#[test]
fn removing_a_value_records_the_before() {
    let registry = mixed_registry();
    let store = MemoryStore::new();
    store.put(
        &ScopeRef::Fleet,
        "runtime.jobs.workers",
        StoredValue::Value(ParamValue::Int(4)),
    );
    let set = ChangeSet::new().push_change(Change::remove(ScopeRef::Fleet, "runtime.jobs.workers"));
    let applied = apply_changes(&set, &registry, &store).unwrap();
    assert_eq!(
        applied[0].before,
        Some(EffectiveValue::Value(ParamValue::Int(4)))
    );
    assert_eq!(applied[0].after, None);
}

/// A store that counts batch applies and individual puts, to prove `apply_changes` batches.
#[derive(Default)]
struct SpyStore {
    inner: MemoryStore,
    applies: AtomicUsize,
    puts: AtomicUsize,
}

impl ValueStore for SpyStore {
    fn get(&self, scope: &ScopeRef, key: &str) -> Option<StoredValue> {
        self.inner.get(scope, key)
    }

    fn put(&self, scope: &ScopeRef, key: &str, value: StoredValue) {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put(scope, key, value);
    }

    fn remove(&self, scope: &ScopeRef, key: &str) {
        self.inner.remove(scope, key);
    }

    fn list(&self, scope: &ScopeRef) -> Vec<String> {
        self.inner.list(scope)
    }

    fn apply(
        &self,
        writes: &[SettingWrite],
    ) -> Result<Vec<AppliedWrite>, crate::error::SettingsError> {
        self.applies.fetch_add(1, Ordering::SeqCst);
        self.inner.apply(writes)
    }
}
