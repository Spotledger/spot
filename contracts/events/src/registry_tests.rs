//! Tests of the event class registry (WP-CON-04.T5).

use std::collections::BTreeSet;

use serde_json::Value as Json;

use crate::registry::{
    Class, Coalescing, Publishable, REGISTRY, REGISTRY_LEN, Retention, check_publishable, class_of,
    lookup, retention_of,
};
use crate::{identity, lifecycle, system};

const SNAPSHOT: &str = include_str!("../registry.snapshot.json");

fn snapshot() -> Vec<Json> {
    let doc: Json = serde_json::from_str(SNAPSHOT).unwrap_or_else(|e| panic!("snapshot: {e}"));
    doc.get("entries")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_else(|| panic!("snapshot has no entries array"))
}

fn text<'a>(v: &'a Json, key: &str) -> &'a str {
    v.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("snapshot entry lacks `{key}`: {v}"))
}

fn as_json<T: serde::Serialize + ?Sized>(v: &T) -> Json {
    serde_json::to_value(v).unwrap_or_else(|e| panic!("serialise: {e}"))
}

#[test]
fn the_registry_has_unique_valid_topics() {
    assert_eq!(REGISTRY.len(), REGISTRY_LEN);
    let mut seen = BTreeSet::new();
    for e in &REGISTRY {
        assert!(seen.insert(e.topic), "duplicate topic {}", e.topic);
        assert!(
            crate::names::Topic::any(e.topic).is_ok(),
            "{} is not a valid topic",
            e.topic
        );
        assert!(!e.owner.is_empty() && e.owner.starts_with("spot-"));
        assert!(e.since.split('.').count() == 3);
        if e.coalescing == Coalescing::LatestPerSubject {
            assert_eq!(
                e.class,
                Class::Domain,
                "{} coalesces but is not Domain",
                e.topic
            );
        }
        // An audit record is kept per the audit retention and nothing else is.
        assert_eq!(
            e.class == Class::Audit,
            e.retention == Retention::Audit,
            "{}: class and retention disagree",
            e.topic
        );
    }
}

#[test]
fn every_schema_the_contract_registers_has_an_entry_and_the_reverse() {
    let mut schema_topics = BTreeSet::new();
    for s in lifecycle::schemas()
        .into_iter()
        .chain(identity::schemas())
        .chain(system::schemas())
        .chain([crate::state::schema()])
    {
        schema_topics.insert(s.topic.as_str().to_owned());
    }
    assert_eq!(schema_topics.len(), 60);
    for t in &schema_topics {
        assert!(
            lookup(t).is_some(),
            "{t} has a schema but no registry entry"
        );
    }
    // The other way: only these registry entries have no schema (their payload is registered by
    // the producing crate or not at all yet). None of them is a `sys_*` topic.
    let schemaless: BTreeSet<&str> = REGISTRY
        .iter()
        .map(|e| e.topic)
        .filter(|t| !schema_topics.contains(*t))
        .collect();
    // 0.7.2/0.7.3/0.7.6: the nine `sys_*` topics, `doc_state` and the five journal/sync audit
    // topics now have a schema; what is left is the retired `doc_edge`.
    let expected: BTreeSet<&str> = ["doc_edge"].into_iter().collect();
    assert_eq!(schemaless, expected);
    assert!(schemaless.iter().all(|t| !t.starts_with("sys_")));
}

#[test]
fn every_identity_topic_is_audit() {
    let mut n = 0;
    for t in identity::TOPICS.iter().chain(identity::NODE_TOPICS.iter()) {
        assert_eq!(class_of(t), Some(Class::Audit), "{t}");
        assert_eq!(retention_of(t), Some(Retention::Audit), "{t}");
        n += 1;
    }
    assert_eq!(n, 35);
    assert_eq!(
        REGISTRY
            .iter()
            .filter(|e| e.topic.starts_with("identity_"))
            .count(),
        35
    );
}

#[test]
fn the_decided_classes() {
    let want = [
        ("sys_partition_range_low", Class::Domain),
        ("sys_condition_raised", Class::Audit),
        ("sys_condition_cleared", Class::Audit),
        ("sys_graph_edge", Class::Domain),
        ("sys_doc_audit", Class::Audit),
        ("sys_secret_audit", Class::Audit),
        ("sys_job_enqueued", Class::Domain),
        ("sys_user_notice", Class::Domain),
        ("doc_state", Class::State),
        ("doc_posted", Class::Domain),
        ("sys_journal_access", Class::Audit),
        ("sys_journal_grant", Class::Audit),
        ("sys_journal_admin", Class::Audit),
        ("sys_sync_node_admin", Class::Audit),
        ("sys_sync_decision", Class::Audit),
    ];
    for (topic, class) in want {
        assert_eq!(class_of(topic), Some(class), "{topic}");
    }
    // `doc_edge` is retired in favour of `sys_graph_edge` and is kept.
    let edge = lookup("doc_edge");
    assert!(edge.is_some_and(|e| e.retired && e.class == Class::Domain));
    assert!(lookup("sys_graph_edge").is_some_and(|e| !e.retired));
    assert_eq!(REGISTRY.iter().filter(|e| e.retired).count(), 1);
    // The rebuild applies State and Audit only.
    assert!(Class::State.applied_by_rebuild());
    assert!(Class::Audit.applied_by_rebuild());
    assert!(!Class::Domain.applied_by_rebuild());
}

#[test]
#[allow(deprecated)]
fn the_old_functions_agree_with_the_registry() {
    for t in system::TOPICS {
        assert_eq!(system::retention_of(t), retention_of(t), "{t}");
        assert_eq!(system::is_audit(t), crate::registry::is_audit(t), "{t}");
    }
    for t in identity::TOPICS.iter().chain(identity::NODE_TOPICS.iter()) {
        assert_eq!(identity::is_audit(t), crate::registry::is_audit(t), "{t}");
    }
    // They answer only for their own family, as before.
    assert_eq!(system::retention_of("doc_posted"), None);
    assert!(!system::is_audit("identity_user_created"));
    assert!(!identity::is_audit("sys_doc_audit"));
    assert!(!identity::is_audit("unknown_topic"));
}

#[test]
fn publishing_refuses_an_unregistered_platform_topic_and_warns_for_an_app_topic() {
    assert_eq!(
        check_publishable("identity_user_created"),
        Ok(Publishable::Registered(Class::Audit))
    );
    assert_eq!(
        check_publishable("sys_graph_edge"),
        Ok(Publishable::Registered(Class::Domain))
    );
    for t in [
        "sys_login_failed",
        "identity_changed",
        "doc_unheard_of",
        "Not A Topic",
        "",
    ] {
        let Err(refused) = check_publishable(t) else {
            panic!("{t} must be refused");
        };
        assert_eq!(refused.topic, t);
        assert!(refused.to_string().contains("no registered event class"));
    }
    assert_eq!(
        check_publishable("stock_below_reorder_level"),
        Ok(Publishable::Warn)
    );
}

#[test]
fn entry_serde_is_stable() {
    let e = lookup("sys_partition_range_low").unwrap_or_else(|| panic!("registered"));
    assert_eq!(
        as_json(e),
        serde_json::json!({
            "topic": "sys_partition_range_low",
            "class": "domain",
            "retention": "outbox",
            "coalescing": "keep_all",
            "owner": "spot-partition",
            "since": "0.7.1",
            "retired": false
        })
    );
}

/// The committed snapshot is the release record: a class change or a removal fails here, and a
/// new entry fails until it is added to the snapshot in the same change.
#[test]
fn the_registry_matches_the_committed_snapshot() {
    let snap = snapshot();
    let mut snap_topics = BTreeSet::new();
    for s in &snap {
        let topic = text(s, "topic");
        snap_topics.insert(topic.to_owned());
        let Some(e) = lookup(topic) else {
            panic!("snapshot topic {topic} was removed from the registry; mark it retired instead");
        };
        let now = as_json(e);
        for key in ["class", "retention", "coalescing", "owner", "since"] {
            assert_eq!(
                text(s, key),
                text(&now, key),
                "{topic}: `{key}` changed after release; a changed class is a new topic name"
            );
        }
        let was_retired = s.get("retired").and_then(Json::as_bool).unwrap_or(false);
        assert!(
            !was_retired || e.retired,
            "{topic} was retired and is no longer"
        );
    }
    for e in &REGISTRY {
        assert!(
            snap_topics.contains(e.topic),
            "{} is registered but not in registry.snapshot.json; add it there",
            e.topic
        );
    }
}

/// Regenerates the snapshot text: `cargo test -p spot-events-contract print_snapshot -- --ignored
/// --nocapture`. Used once when a topic is added; a reviewer reads the diff.
#[test]
#[ignore = "prints the snapshot to paste into registry.snapshot.json"]
fn print_snapshot() {
    let doc = serde_json::json!({ "contract_version": crate::CONTRACT_VERSION, "entries": as_json(&REGISTRY[..]) });
    println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
}
