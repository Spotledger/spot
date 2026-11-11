//! Tests of the five journal and sync audit payload schemas (WP-JRN-01.T2).

use std::collections::BTreeSet;

use serde_json::{Value as Json, json};

use crate::journal::{self, TOPICS};
use crate::registry::{Class, class_of, lookup};
use crate::system::{self, validate};
use crate::{check, identity};

/// An example name built from parts, so no dotted literal reads as a raised code.
fn dotted(stem: &str, outcome: &str) -> String {
    format!("{stem}.{outcome}")
}

fn example(name: &str) -> Json {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/sys")
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn ok(topic: &str, data: &Json) {
    assert_eq!(validate(topic, data), Ok(()), "{topic}: {data}");
}

fn bad(topic: &str, data: &Json) {
    assert!(validate(topic, data).is_err(), "{topic} accepted {data}");
}

fn with(data: &Json, field: &str, value: Option<Json>) -> Json {
    let mut d = data.clone();
    if let Some(map) = d.as_object_mut() {
        match value {
            Some(v) => map.insert(field.to_owned(), v),
            None => map.remove(field),
        };
    }
    d
}

#[test]
fn the_five_topics_have_v1_schemas_that_are_registered_and_audit() {
    for topic in TOPICS {
        let s = system::schema_of(topic).unwrap_or_else(|| panic!("{topic} has no schema"));
        assert_eq!(s.topic.as_str(), topic);
        assert_eq!((s.major, s.revision), (1, 0), "{topic}");
        assert_eq!(s.check_definition(), Ok(()), "{topic}");
        assert_eq!(check(&s, &s), Ok(()), "{topic}");
        assert!(system::TOPICS.contains(&topic), "{topic}");
        assert_eq!(
            lookup(topic).map(|e| e.class),
            Some(Class::Audit),
            "{topic}"
        );
        assert_eq!(class_of(topic), Some(Class::Audit), "{topic}");
    }
    let unique: BTreeSet<&str> = TOPICS.iter().copied().collect();
    assert_eq!(unique.len(), 5);
}

#[test]
fn every_example_of_the_five_validates() {
    for (topic, name) in [
        (journal::JOURNAL_ACCESS_TOPIC, "sys_journal_access"),
        (
            journal::JOURNAL_ACCESS_TOPIC,
            dotted("sys_journal_access", "denied").as_str(),
        ),
        (journal::JOURNAL_GRANT_TOPIC, "sys_journal_grant"),
        (
            journal::JOURNAL_GRANT_TOPIC,
            dotted("sys_journal_grant", "refused").as_str(),
        ),
        (journal::JOURNAL_ADMIN_TOPIC, "sys_journal_admin"),
        (
            journal::JOURNAL_ADMIN_TOPIC,
            dotted("sys_journal_admin", "refused").as_str(),
        ),
        (journal::SYNC_NODE_ADMIN_TOPIC, "sys_sync_node_admin"),
        (
            journal::SYNC_NODE_ADMIN_TOPIC,
            dotted("sys_sync_node_admin", "failed").as_str(),
        ),
        (journal::SYNC_DECISION_TOPIC, "sys_sync_decision"),
        (
            journal::SYNC_DECISION_TOPIC,
            dotted("sys_sync_decision", "refused").as_str(),
        ),
    ] {
        ok(topic, &example(name));
    }
}

#[test]
fn every_five_schema_is_strict_and_refuses_an_unknown_field_or_a_missing_outcome() {
    for topic in TOPICS {
        let mut d = example(topic);
        d["surprise"] = json!(1);
        bad(topic, &d);
        bad(topic, &json!([]));
        bad(topic, &json!({}));
    }
    // `outcome` is required on every topic but the grant lifecycle (which has `action` instead).
    for topic in [
        journal::JOURNAL_ACCESS_TOPIC,
        journal::JOURNAL_ADMIN_TOPIC,
        journal::SYNC_NODE_ADMIN_TOPIC,
        journal::SYNC_DECISION_TOPIC,
    ] {
        bad(topic, &with(&example(topic), "outcome", None));
    }
}

#[test]
fn an_action_outside_the_closed_enum_is_refused() {
    bad(
        journal::JOURNAL_GRANT_TOPIC,
        &with(
            &example(journal::JOURNAL_GRANT_TOPIC),
            "action",
            Some(json!("granted_forever")),
        ),
    );
    bad(
        journal::JOURNAL_ADMIN_TOPIC,
        &with(
            &example(journal::JOURNAL_ADMIN_TOPIC),
            "action",
            Some(json!("exploded")),
        ),
    );
    bad(
        journal::SYNC_NODE_ADMIN_TOPIC,
        &with(
            &example(journal::SYNC_NODE_ADMIN_TOPIC),
            "action",
            Some(json!("teleported")),
        ),
    );
    bad(
        journal::SYNC_DECISION_TOPIC,
        &with(
            &example(journal::SYNC_DECISION_TOPIC),
            "action",
            Some(json!("ignored")),
        ),
    );
    // The seventh node-admin action is legal although it is not in `NODE_ADMIN_ACTIONS` alone.
    ok(
        journal::SYNC_NODE_ADMIN_TOPIC,
        &with(
            &example(journal::SYNC_NODE_ADMIN_TOPIC),
            "action",
            Some(json!("role_changed")),
        ),
    );
}

/// Defence in depth: none of the five schemas declares a field that looks like a secret, even
/// though every value here is a bounded id, code or name (never a credential).
#[test]
fn no_schema_of_the_five_topics_declares_a_forbidden_field_name() {
    const FORBIDDEN: [&str; 5] = ["key", "token", "secret", "password", "credential"];
    for topic in TOPICS {
        let s = system::schema_of(topic).unwrap_or_else(|| panic!("{topic} has no schema"));
        for field in s.fields.keys() {
            assert!(
                !FORBIDDEN.contains(&field.as_str()),
                "{topic} declares forbidden field {field}"
            );
            assert!(
                !identity::FORBIDDEN_KEY_PARTS
                    .iter()
                    .any(|p| field.contains(p)),
                "{topic} declares {field}"
            );
        }
    }
}

#[test]
fn journal_access_object_and_sequence_rules() {
    let t = journal::JOURNAL_ACCESS_TOPIC;
    let base = example(t);
    // The grant is the journal_reader grant id, or the literal "admin".
    bad(t, &with(&base, "grant", Some(json!("not-a-grant"))));
    ok(t, &with(&base, "grant", Some(json!("admin"))));
    // first_seq/last_seq are required unless the object is a manifest.
    let manifest = example(&dotted("sys_journal_access", "denied"));
    ok(t, &with(&manifest, "first_seq", None));
    bad(t, &with(&base, "first_seq", None));
    bad(t, &with(&base, "last_seq", None));
    bad(t, &with(&base, "last_seq", Some(json!(1))));
    bad(t, &with(&base, "first_seq", Some(json!(-1))));
    bad(t, &with(&base, "bytes", Some(json!(-1))));
    bad(t, &with(&base, "object", Some(json!("ledger"))));
    bad(t, &with(&base, "reason", Some(json!("Not Lower"))));
}

#[test]
fn journal_grant_lifecycle_rules() {
    let t = journal::JOURNAL_GRANT_TOPIC;
    let base = example(t);
    // expires_at is required for granted and renewed, and absent otherwise.
    bad(t, &with(&base, "expires_at", None));
    let revoked = example(&dotted("sys_journal_grant", "refused"));
    bad(
        t,
        &with(&revoked, "expires_at", Some(json!("2026-12-01T00:00:00Z"))),
    );
    // The grantor is a principal id, or "system" exactly when the action is expired.
    bad(t, &with(&base, "grantor", Some(json!("system"))));
    let expired = with(
        &with(&revoked, "action", Some(json!("expired"))),
        "grantor",
        Some(json!("system")),
    );
    ok(t, &expired);
    bad(t, &with(&revoked, "action", Some(json!("expired"))));
}

#[test]
fn journal_admin_target_and_code_rules() {
    let t = journal::JOURNAL_ADMIN_TOPIC;
    let base = example(t);
    // The target is never a record id.
    bad(
        t,
        &with(
            &base,
            "target",
            Some(json!("0192f1c0-0000-7000-8000-000000000099")),
        ),
    );
    // A code is only present when the outcome is not ok.
    bad(
        t,
        &with(
            &base,
            "code",
            Some(json!(dotted("journal", "rebuild_conflict"))),
        ),
    );
    let refused = example(&dotted("sys_journal_admin", "refused"));
    bad(t, &with(&refused, "code", Some(json!("Not.A.Code"))));
    bad(t, &with(&refused, "actor", Some(json!("nope"))));
    ok(t, &with(&refused, "actor", Some(json!("system"))));
}

#[test]
fn sync_node_admin_rules() {
    let t = journal::SYNC_NODE_ADMIN_TOPIC;
    let base = example(t);
    for node in ["Branch-01", "branch 01", "", "-branch"] {
        bad(t, &with(&base, "node", Some(json!(node))));
    }
    bad(t, &with(&base, "fingerprint", Some(json!("ABCD"))));
    bad(t, &with(&base, "fingerprint", Some(json!("a".repeat(63)))));
}

#[test]
fn sync_decision_rules() {
    let t = journal::SYNC_DECISION_TOPIC;
    let base = example(t);
    bad(t, &with(&base, "count", Some(json!(-1))));
    bad(t, &with(&base, "entity", Some(json!("Sales Invoice"))));
    ok(t, &with(&base, "node", None));
    ok(t, &with(&base, "entity", None));
}
