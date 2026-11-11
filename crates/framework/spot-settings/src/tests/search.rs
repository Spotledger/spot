//! Search: ranking, folding, filters and the comparison budget.

use spot_mgmt_ops::{ParamValue, Risk};

use crate::model::{Restart, Scope, ScopeRef, StoredValue};
use crate::resolution::Chain;
use crate::search::{Filter, MatchedField, search, search_counted};
use crate::spec::SettingSpec;
use crate::store::{MemoryStore, ValueStore};
use crate::tests::{build, int_spec, spec};

fn runtime_spec(key: &str, title: &str, summary: &str, tags: &[&str]) -> SettingSpec {
    let mut spec = spec(
        key,
        spot_mgmt_ops::ParamType::Bool,
        Some(ParamValue::Bool(false)),
        vec![Scope::Fleet],
    );
    spec.title = title.to_owned();
    spec.summary = summary.to_owned();
    spec.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
    spec
}

fn ranking_registry() -> crate::builder::SettingsRegistry {
    build(vec![
        runtime_spec("exact.key", "Exact", "An exact key match.", &[]),
        runtime_spec("prefix.thing", "Prefix Thing", "A prefix.", &[]),
        runtime_spec("runtime.backup", "Backup Retention", "How long.", &[]),
        runtime_spec(
            "runtime.logs",
            "Logs",
            "Controls the verbosity of logs.",
            &[],
        ),
        runtime_spec(
            "runtime.telemetry",
            "Telemetry",
            "Metrics.",
            &["observability"],
        ),
        runtime_spec("runtime.cafe", "Café Settings", "Accented.", &[]),
    ])
}

fn empty_chain() -> Chain {
    Chain::new(Vec::new()).unwrap()
}

fn empty_store() -> MemoryStore {
    MemoryStore::new()
}

#[test]
fn the_ranking_is_exact_then_prefix_then_title_then_summary_then_tag() {
    let registry = ranking_registry();
    let chain = empty_chain();
    let store = empty_store();
    let filters = Filter::default();

    let exact = search("exact.key", &registry, &chain, &store, &filters);
    assert_eq!(exact[0].key, "exact.key");
    assert_eq!(exact[0].matched_field, MatchedField::ExactKey);

    let prefix = search("prefix", &registry, &chain, &store, &filters);
    assert_eq!(prefix[0].key, "prefix.thing");
    assert_eq!(prefix[0].matched_field, MatchedField::KeyPrefix);

    let title = search("retention", &registry, &chain, &store, &filters);
    assert_eq!(title[0].key, "runtime.backup");
    assert_eq!(title[0].matched_field, MatchedField::TitleWord);

    let summary = search("verbosity", &registry, &chain, &store, &filters);
    assert_eq!(summary[0].key, "runtime.logs");
    assert_eq!(summary[0].matched_field, MatchedField::SummaryWord);

    let tag = search("observability", &registry, &chain, &store, &filters);
    assert_eq!(tag[0].key, "runtime.telemetry");
    assert_eq!(tag[0].matched_field, MatchedField::Tag);
}

#[test]
fn matching_is_case_insensitive_and_accent_folded() {
    let registry = ranking_registry();
    let chain = empty_chain();
    let store = empty_store();
    let filters = Filter::default();
    let hits = search("CAFE", &registry, &chain, &store, &filters);
    assert_eq!(hits[0].key, "runtime.cafe");
}

#[test]
fn an_empty_query_returns_nothing() {
    let registry = ranking_registry();
    let chain = empty_chain();
    let store = empty_store();
    assert!(search("   ", &registry, &chain, &store, &Filter::default()).is_empty());
}

#[test]
fn ties_break_on_the_key() {
    let registry = build(vec![
        runtime_spec("runtime.alpha", "Shared Word", "x", &[]),
        runtime_spec("runtime.beta", "Shared Word", "y", &[]),
    ]);
    let chain = empty_chain();
    let store = empty_store();
    let hits = search("shared", &registry, &chain, &store, &Filter::default());
    let keys: Vec<&str> = hits.iter().map(|hit| hit.key.as_str()).collect();
    assert_eq!(keys, vec!["runtime.alpha", "runtime.beta"]);
}

#[test]
fn filters_narrow_the_result() {
    let mut changed = int_spec(
        "runtime.jobs.workers",
        1,
        64,
        4,
        vec![Scope::Fleet, Scope::Server],
    );
    changed.risk = Risk::High;
    changed.restart = Restart::Service {
        name: "jobs".to_owned(),
    };
    let quiet = int_spec("runtime.other", 1, 64, 4, vec![Scope::Fleet]);
    let registry = build(vec![changed, quiet]);
    let chain = Chain::new(vec![ScopeRef::Fleet, ScopeRef::Server("srv".to_owned())]).unwrap();
    let store = MemoryStore::new();
    store.put(
        &ScopeRef::Server("srv".to_owned()),
        "runtime.jobs.workers",
        StoredValue::Value(ParamValue::Int(9)),
    );

    // Both keys share "runtime." so search that prefix and filter down.
    let by_risk = search(
        "runtime",
        &registry,
        &chain,
        &store,
        &Filter {
            risk: Some(Risk::High),
            ..Filter::default()
        },
    );
    assert_eq!(by_risk.len(), 1);
    assert_eq!(by_risk[0].key, "runtime.jobs.workers");

    let by_scope = search(
        "runtime",
        &registry,
        &chain,
        &store,
        &Filter {
            scope: Some(Scope::Server),
            ..Filter::default()
        },
    );
    assert_eq!(by_scope.len(), 1);

    let by_restart = search(
        "runtime",
        &registry,
        &chain,
        &store,
        &Filter {
            restart: Some(Restart::Service {
                name: "jobs".to_owned(),
            }),
            ..Filter::default()
        },
    );
    assert_eq!(by_restart.len(), 1);

    let changed_only = search(
        "runtime",
        &registry,
        &chain,
        &store,
        &Filter {
            only_changed: true,
            ..Filter::default()
        },
    );
    assert_eq!(changed_only.len(), 1);
    assert_eq!(changed_only[0].key, "runtime.jobs.workers");

    let at_scope = search(
        "runtime",
        &registry,
        &chain,
        &store,
        &Filter {
            only_modified_at_scope: Some(ScopeRef::Server("srv".to_owned())),
            ..Filter::default()
        },
    );
    assert_eq!(at_scope.len(), 1);

    let by_area = search(
        "runtime",
        &registry,
        &chain,
        &store,
        &Filter {
            area: Some("runtime".to_owned()),
            ..Filter::default()
        },
    );
    assert_eq!(by_area.len(), 2);
}

#[test]
fn a_large_registry_stays_within_the_comparison_budget() {
    // 2,000 settings; a single-token query compares at most a small constant per setting.
    let specs: Vec<SettingSpec> = (0..2_000)
        .map(|index| {
            let mut spec = spec(
                &format!("runtime.setting{index}"),
                spot_mgmt_ops::ParamType::Bool,
                Some(ParamValue::Bool(false)),
                vec![Scope::Fleet],
            );
            spec.title = format!("Setting {index}");
            spec.summary = "A synthetic setting.".to_owned();
            spec
        })
        .collect();
    let registry = build(specs);
    let chain = empty_chain();
    let store = empty_store();

    let mut comparisons = 0u64;
    let hits = search_counted(
        "setting",
        &registry,
        &chain,
        &store,
        &Filter::default(),
        &mut comparisons,
    );
    assert_eq!(hits.len(), 2_000);

    // Documented budget: at most 10 comparisons per setting per token, plus the whole-query
    // check. 2,000 settings * 10 = 20,000; assert a generous 250,000.
    assert!(
        comparisons <= 250_000,
        "comparisons {comparisons} exceeded the documented budget"
    );
}
