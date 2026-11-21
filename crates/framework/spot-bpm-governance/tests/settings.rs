//! The settings-backed policy: the mode and the reason floor read through `spot-settings`, and
//! effective-dating.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_bpm_governance::{
    CreateAttempt, GovernanceDecision, GovernanceError, GovernanceGuard, GovernanceMode,
    MemoryRequests, POLICY_KEY, PolicyChange, PolicyStore, REASON_MIN_KEY, RecordingAudit,
    RefusalReason, RoleOutsideProcess, SettingsPolicy, policy_registry, setting_specs,
};
use spot_settings::{MemoryStore, ParamValue, ScopeRef, SettingsRegistry, StoredValue, ValueStore};

fn registry() -> Arc<SettingsRegistry> {
    Arc::new(policy_registry().unwrap())
}

fn store() -> Arc<MemoryStore> {
    Arc::new(MemoryStore::new())
}

fn policy(registry: &Arc<SettingsRegistry>, store: &Arc<MemoryStore>) -> SettingsPolicy {
    SettingsPolicy::new(registry.clone(), store.clone(), "tenant-1")
}

fn set_mode(store: &MemoryStore, company: &str, doctype: &str, mode: &str) {
    store.put(
        &ScopeRef::Company(company.to_owned()),
        POLICY_KEY,
        StoredValue::Value(ParamValue::List(vec![ParamValue::Text(format!(
            "{doctype}={mode}"
        ))])),
    );
}

#[test]
fn the_registry_builds_clean() {
    let registry = policy_registry().unwrap();
    assert!(registry.contains_key(POLICY_KEY));
    assert!(registry.contains_key(REASON_MIN_KEY));
    assert_eq!(setting_specs().len(), 2);
}

#[test]
fn free_by_default() {
    let registry = registry();
    let store = store();
    let policy = policy(&registry, &store);
    assert_eq!(
        policy.current("acme", "note").unwrap().mode,
        GovernanceMode::Free
    );
    assert_eq!(
        policy.mode_at("acme", "note", now()).unwrap(),
        GovernanceMode::Free
    );
}

#[test]
fn governed_is_read_per_company() {
    let registry = registry();
    let store = store();
    set_mode(&store, "acme", "note", "governed");
    let policy = policy(&registry, &store);
    assert_eq!(
        policy.current("acme", "note").unwrap().mode,
        GovernanceMode::Governed
    );
    // A different company is untouched: turning governance on for one changes nothing for
    // another.
    assert_eq!(
        policy.current("other", "note").unwrap().mode,
        GovernanceMode::Free
    );
    // A different record type in the same company is untouched.
    assert_eq!(
        policy.current("acme", "customer").unwrap().mode,
        GovernanceMode::Free
    );
}

#[test]
fn the_reason_floor_defaults_and_is_overridable() {
    let registry = registry();
    let store = store();
    let policy = policy(&registry, &store);
    assert_eq!(policy.min_reason_chars("acme").unwrap(), 8);
    store.put(
        &ScopeRef::Company("acme".to_owned()),
        REASON_MIN_KEY,
        StoredValue::Value(ParamValue::Int(20)),
    );
    assert_eq!(policy.min_reason_chars("acme").unwrap(), 20);
    assert_eq!(policy.min_reason_chars("other").unwrap(), 8);
}

#[test]
fn a_malformed_policy_is_refused() {
    let registry = registry();
    let store = store();
    set_mode(&store, "acme", "note", "loud");
    let policy = policy(&registry, &store);
    let error = policy.current("acme", "note").unwrap_err();
    assert!(matches!(error, GovernanceError::PolicyInvalid { .. }));
}

#[test]
fn a_policy_change_is_effective_dated() {
    let registry = registry();
    let store = store();
    let policy = policy(&registry, &store);
    policy
        .record_change(PolicyChange::new(
            "acme",
            "note",
            GovernanceMode::Free,
            GovernanceMode::Governed,
            later(3600),
            "admin",
            now(),
        ))
        .unwrap();
    // Before the effective instant the earlier mode still holds; at and after it the new one.
    assert_eq!(
        policy.mode_at("acme", "note", now()).unwrap(),
        GovernanceMode::Free
    );
    assert_eq!(
        policy.mode_at("acme", "note", later(3600)).unwrap(),
        GovernanceMode::Governed
    );
}

#[test]
fn the_guard_reads_the_policy_through_settings() {
    let registry = registry();
    let store = store();
    set_mode(&store, "acme", "note", "governed");
    let policy = Arc::new(policy(&registry, &store));
    let requests = Arc::new(MemoryRequests::new());
    let audit = Arc::new(RecordingAudit::new());
    let guard = GovernanceGuard::new(
        policy,
        requests,
        Arc::new(RoleOutsideProcess),
        audit.clone(),
    );
    let actor = user_actor();
    let principal = plain_principal();
    let attempt = CreateAttempt {
        company: "acme",
        doctype: "note",
        document_id: spot_app_api::prelude::RecordId::new_v7(),
        actor: &actor,
        principal: &principal,
        reason: None,
        request: None,
        command_id: None,
        now: now(),
    };
    assert_eq!(
        guard.before_create(&attempt).unwrap(),
        GovernanceDecision::Refuse {
            reason: RefusalReason::NoPermission
        }
    );
    assert_eq!(audit.len(), 1);
}
