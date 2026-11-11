//! Retention from outside the crate: per-kind TTLs and caps, deterministic compaction into
//! aggregate statistics, legal holds, and tenant export and deletion.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_agent_memory::testing::{embedder, episode_saying, fact, procedure};
use spot_agent_memory::{
    DAY_MS, Entry, Epoch, FactValue, InMemoryStore, KindPolicy, MemoryKind, MemoryStore,
    RetentionPolicy, TenantId, apply_retention, delete_tenant, export_tenant, plan_retention,
};
use spot_data_policy::Holds;

fn tenant() -> TenantId {
    TenantId::parse("tenant_a").expect("a well-formed tenant")
}

fn episodes(store: &InMemoryStore) {
    for (n, day) in [(1, 1), (2, 2), (3, 3)] {
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                n,
                "the job stayed stuck and a retry cleared it",
                i64::from(day) * DAY_MS,
            )))
            .unwrap();
    }
}

#[test]
fn an_expired_entry_is_planned_for_expiry_and_removed() {
    let store = InMemoryStore::new();
    episodes(&store);
    let policy = RetentionPolicy::default();
    let plan = plan_retention(&store, &tenant(), &policy, 100 * DAY_MS, &Holds::empty()).unwrap();
    assert_eq!(
        plan.expiries().len(),
        3,
        "the 90-day episode TTL has passed"
    );
    assert!(plan.compactions().is_empty());

    let report = apply_retention(&store, &embedder(), &plan, Epoch::new(1)).unwrap();
    assert_eq!(report.expired.len(), 3);
    assert_eq!(store.count(&tenant(), MemoryKind::Episode), 0);
}

#[test]
fn a_fresh_entry_is_left_alone() {
    let store = InMemoryStore::new();
    episodes(&store);
    let policy = RetentionPolicy::default();
    let plan = plan_retention(&store, &tenant(), &policy, 10 * DAY_MS, &Holds::empty()).unwrap();
    assert!(plan.is_empty());
    assert!(
        apply_retention(&store, &embedder(), &plan, Epoch::new(1))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_size_cap_folds_the_oldest_episodes_into_statistics() {
    let store = InMemoryStore::new();
    episodes(&store);
    let policy = RetentionPolicy {
        episode: KindPolicy::new(None, 1),
        compaction_min_age_days: 0,
        ..RetentionPolicy::default()
    };
    let plan = plan_retention(&store, &tenant(), &policy, 4 * DAY_MS, &Holds::empty()).unwrap();
    assert_eq!(plan.compactions().len(), 2);

    let report = apply_retention(&store, &embedder(), &plan, Epoch::new(3)).unwrap();
    assert_eq!(report.compacted.len(), 2);
    assert_eq!(report.facts.len(), 1);
    assert_eq!(
        store.count(&tenant(), MemoryKind::Episode),
        1,
        "the cap holds"
    );
    assert_eq!(store.count(&tenant(), MemoryKind::Fact), 1);

    let facts = store.list(&tenant(), MemoryKind::Fact);
    match &facts[0] {
        Entry::Fact(fact) => {
            assert_eq!(fact.epoch, Epoch::new(3), "the aggregate names its epoch");
            assert_eq!(fact.meta.source.as_str(), "compaction");
            match &fact.value {
                FactValue::Stats(aggregate) => {
                    assert_eq!(aggregate.count, 2);
                    assert_eq!(aggregate.first_ms, DAY_MS);
                    assert_eq!(aggregate.last_ms, 2 * DAY_MS);
                    assert_eq!(aggregate.outcome.as_str(), "resolved");
                }
                other => panic!("compaction writes statistics, not {other:?}"),
            }
        }
        other => panic!("compaction writes a fact, not {other:?}"),
    }
}

#[test]
fn compaction_is_deterministic_and_merges_on_a_second_run() {
    let store = InMemoryStore::new();
    episodes(&store);
    let policy = RetentionPolicy {
        episode: KindPolicy::new(None, 1),
        compaction_min_age_days: 0,
        ..RetentionPolicy::default()
    };
    let first = plan_retention(&store, &tenant(), &policy, 4 * DAY_MS, &Holds::empty()).unwrap();
    let again = plan_retention(&store, &tenant(), &policy, 4 * DAY_MS, &Holds::empty()).unwrap();
    assert_eq!(first, again, "the same store and policy give the same plan");

    let report = apply_retention(&store, &embedder(), &first, Epoch::new(3)).unwrap();
    assert_eq!(report.facts.len(), 1);
    let before = store.list(&tenant(), MemoryKind::Fact);
    // A second, empty plan changes nothing.
    let empty = plan_retention(&store, &tenant(), &policy, 4 * DAY_MS, &Holds::empty()).unwrap();
    let report = apply_retention(&store, &embedder(), &empty, Epoch::new(4)).unwrap();
    assert!(report.facts.is_empty());
    assert_eq!(store.list(&tenant(), MemoryKind::Fact), before);
}

#[test]
fn a_legal_hold_suspends_retention_and_survives_tenant_deletion() {
    let store = InMemoryStore::new();
    episodes(&store);
    store
        .put(Entry::Procedure(procedure("tenant_a", 1)))
        .unwrap();
    let holds = Holds::from_toml(
        "[[hold]]\nid = \"litigation-1\"\nreason = \"preserve the incidents\"\n\
         set_by = \"user:legal-1\"\nscope = \"doctype:agent_memory_episode\"\n",
    )
    .expect("a well-formed hold");

    let plan = plan_retention(
        &store,
        &tenant(),
        &RetentionPolicy::default(),
        100 * DAY_MS,
        &holds,
    )
    .unwrap();
    assert_eq!(plan.holds().len(), 3);
    assert!(plan.expiries().is_empty(), "a hold beats a TTL");

    let report = delete_tenant(&store, &tenant(), &holds).unwrap();
    assert_eq!(report.held.len(), 3);
    assert_eq!(report.deleted.len(), 1, "the procedure is not held");
    assert_eq!(store.count(&tenant(), MemoryKind::Episode), 3);
    assert_eq!(store.count(&tenant(), MemoryKind::Procedure), 0);
}

#[test]
fn an_export_carries_exactly_one_tenants_memory() {
    let store = InMemoryStore::new();
    episodes(&store);
    store
        .put(Entry::Procedure(procedure("tenant_a", 1)))
        .unwrap();
    store.put(Entry::Fact(fact("tenant_b", 1))).unwrap();

    let export = export_tenant(&store, &tenant()).unwrap();
    assert_eq!(export.tenant, "tenant_a");
    assert_eq!(export.episodes.len(), 3);
    assert_eq!(export.procedures.len(), 1);
    assert!(
        export.facts.is_empty(),
        "another tenant's fact is not exported"
    );
    let json = export.to_json().unwrap();
    assert!(json.contains("episode.e1"));
    assert!(!json.contains("tenant_b"));
}

#[test]
fn the_policy_is_configurable_toml() {
    let policy = RetentionPolicy {
        episode: KindPolicy::new(Some(14), 100),
        procedure: KindPolicy::new(None, 50),
        fact: KindPolicy::new(Some(1), 10),
        compaction_min_age_days: 2,
    };
    let text = policy.to_toml().unwrap();
    assert_eq!(RetentionPolicy::from_toml(&text).unwrap(), policy);
    assert!(
        RetentionPolicy::from_toml("[episode]\nttl_days = 14\nunknown_key = 1\n").is_err(),
        "an unknown key is refused"
    );
}
