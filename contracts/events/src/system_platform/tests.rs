//! Tests of the nine `sys_*` payload schemas added in 0.7.2 (WP-CON-04.T6).

use std::collections::BTreeSet;

use serde_json::{Value as Json, json};

use crate::registry::{Class, class_of, lookup};
use crate::system::{self, validate};
use crate::system_platform as sp;
use crate::{check, identity, lifecycle};

/// The nine topics, in the order the contract lists them.
const NINE: [&str; 9] = [
    sp::JOB_ENQUEUED_TOPIC,
    sp::USER_NOTICE_TOPIC,
    sp::OPERATION_RECEIPT_TOPIC,
    sp::OPERATION_RECEIPT_EFFECT_TOPIC,
    sp::PARTITION_RANGE_LOW_TOPIC,
    sp::BPM_GOVERNANCE_TOPIC,
    sp::BPM_CASE_MIGRATED_TOPIC,
    sp::CONDITION_RAISED_TOPIC,
    sp::CONDITION_CLEARED_TOPIC,
];

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

/// `data` with `field` replaced (or removed when `value` is `None`).
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
fn the_nine_topics_have_v1_schemas_that_are_well_formed() {
    for topic in NINE {
        let s = system::schema_of(topic).unwrap_or_else(|| panic!("{topic} has no schema"));
        assert_eq!(s.topic.as_str(), topic);
        assert_eq!((s.major, s.revision), (1, 0), "{topic}");
        assert_eq!(s.check_definition(), Ok(()), "{topic}");
        assert_eq!(check(&s, &s), Ok(()), "{topic}");
        assert!(system::TOPICS.contains(&topic), "{topic}");
        assert!(lookup(topic).is_some(), "{topic} is not registered");
    }
    assert_eq!(system::TOPICS.len(), 17);
    assert_eq!(system::schemas().len(), 17);
    let unique: BTreeSet<&str> = system::TOPICS.iter().copied().collect();
    assert_eq!(unique.len(), 17);
}

#[test]
fn the_nine_schemas_leave_the_44_other_schemas_alone() {
    // The 7 lifecycle, 35 identity and three older sys schemas are unchanged: the older fingerprint
    // test in `tests.rs` pins them; here only the count and the untouched classes are checked.
    let all = lifecycle::schemas().len() + identity::schemas().len() + system::schemas().len();
    assert_eq!(all, 7 + 35 + 17);
    assert_eq!(class_of(sp::BPM_GOVERNANCE_TOPIC), Some(Class::Audit));
    assert_eq!(class_of(sp::USER_NOTICE_TOPIC), Some(Class::Domain));
}

#[test]
fn every_example_of_the_nine_validates() {
    for (topic, name) in [
        ("sys_job_enqueued", "sys_job_enqueued"),
        ("sys_job_enqueued", "sys_job_enqueued.minimal"),
        ("sys_user_notice", "sys_user_notice"),
        ("sys_operation_receipt", "sys_operation_receipt"),
        (
            "sys_operation_receipt_effect",
            "sys_operation_receipt_effect",
        ),
        ("sys_partition_range_low", "sys_partition_range_low"),
        ("sys_bpm_governance", "sys_bpm_governance"),
        ("sys_bpm_governance", "sys_bpm_governance.policy_change"),
        ("sys_bpm_case_migrated", "sys_bpm_case_migrated"),
        ("sys_condition_raised", "sys_condition_raised"),
        ("sys_condition_raised", "sys_condition_raised.escalated"),
        ("sys_condition_cleared", "sys_condition_cleared"),
    ] {
        ok(topic, &example(name));
    }
}

#[test]
fn every_nine_schema_is_strict() {
    for topic in NINE {
        let mut d = example(topic);
        d["surprise"] = json!(1);
        bad(topic, &d);
        bad(topic, &json!([]));
        bad(topic, &json!({}));
    }
}

#[test]
fn no_nine_payload_carries_a_secret_key_or_a_long_string() {
    let base = example("sys_job_enqueued");
    bad(
        "sys_job_enqueued",
        &with(&base, "args", Some(json!({"api_token": "x"}))),
    );
    bad(
        "sys_job_enqueued",
        &with(&base, "args", Some(json!({"note": "x".repeat(257)}))),
    );
    bad(
        "sys_user_notice",
        &with(
            &example("sys_user_notice"),
            "payload",
            Some(json!({"password": "x"})),
        ),
    );
    bad(
        "sys_condition_raised",
        &with(
            &example("sys_condition_raised"),
            "params",
            Some(json!({"secret_name": "x"})),
        ),
    );
}

#[test]
fn job_enqueued_rules() {
    let t = "sys_job_enqueued";
    let base = example(t);
    for job in [
        "send_overdue",
        "Erp:job",
        "erp_selling:",
        ":job",
        "a:b:c",
        "",
    ] {
        bad(t, &with(&base, "job", Some(json!(job))));
    }
    bad(t, &with(&base, "job", None));
    // Args are any JSON, but a float is refused (ADR-055: a decimal is a string).
    ok(
        t,
        &with(&base, "args", Some(json!(["a", 1, {"d": "1.50"}]))),
    );
    ok(t, &with(&base, "args", None));
    bad(t, &with(&base, "args", Some(json!({"qty": 1.5}))));
    bad(t, &with(&base, "idempotency_key", Some(json!(""))));
    bad(t, &with(&base, "idempotency_key", Some(json!(7))));
    // The producer keeps the offset the requester gave, so a non-UTC time is valid.
    ok(
        t,
        &with(
            &base,
            "not_before",
            Some(json!("2026-11-03T11:00:00+05:00")),
        ),
    );
    bad(t, &with(&base, "not_before", Some(json!("tomorrow"))));
    // `site` is the intake's own context, never a payload field.
    bad(t, &with(&base, "site", Some(json!("x"))));
    // `queue`: 1 to 64 bytes of a-z, 0-9, `_` and `-`.
    ok(t, &with(&base, "queue", Some(json!("default"))));
    ok(t, &with(&base, "queue", Some(json!("a"))));
    ok(t, &with(&base, "queue", Some(json!("q-1_b".repeat(12)))));
    for queue in ["", "Default", "a b", "a.b", "é"] {
        bad(t, &with(&base, "queue", Some(json!(queue))));
    }
    bad(t, &with(&base, "queue", Some(json!("a".repeat(65)))));
    bad(t, &with(&base, "queue", Some(json!(3))));
    // `context`: an object, no float at any depth, same key and string bounds as `args`.
    ok(t, &with(&base, "context", Some(json!({}))));
    ok(
        t,
        &with(
            &base,
            "context",
            Some(json!({"user": "u", "n": [1, {"d": "1.50"}]})),
        ),
    );
    for context in [
        json!("x"),
        json!([1]),
        json!(3),
        json!({"qty": 1.5}),
        json!({"a": {"b": [2.5]}}),
    ] {
        bad(t, &with(&base, "context", Some(context)));
    }
    bad(t, &with(&base, "context", Some(json!({"api_token": "x"}))));
    bad(
        t,
        &with(&base, "context", Some(json!({"note": "x".repeat(257)}))),
    );
}

#[test]
fn job_enqueued_uses_queue_and_context_in_its_example() {
    let base = example("sys_job_enqueued");
    assert!(base.get("queue").is_some_and(Json::is_string));
    assert!(base.get("context").is_some_and(Json::is_object));
}

#[test]
fn the_engines_literal_job_shape_validates() {
    // `spot-document`'s `engine::outbox::job` writes every optional key, null when absent.
    let t = "sys_job_enqueued";
    ok(
        t,
        &json!({"job": "erp_selling:send_overdue_reminders", "args": null,
                "idempotency_key": null, "not_before": null}),
    );
    // And with every optional key null, including the intake's two.
    ok(
        t,
        &json!({"job": "erp_selling:send_overdue_reminders", "args": null,
                "idempotency_key": null, "not_before": null, "queue": null, "context": null}),
    );
    // A required field is never null.
    bad(t, &json!({"job": null, "args": null}));
}

/// The optional top-level fields of a topic's schema, sorted.
fn optional_fields(topic: &str) -> Vec<String> {
    let schema = system::schema_of(topic).unwrap_or_else(|| panic!("{topic}"));
    schema
        .fields
        .iter()
        .filter(|(_, f)| !f.required)
        .map(|(name, _)| name.clone())
        .collect()
}

#[test]
fn every_nine_schema_states_whether_a_null_is_accepted() {
    // The module doc's null table is the statement; it must match the schema, row by row.
    let source = include_str!("../system_platform.rs");
    for topic in NINE {
        let needle = format!("//! | `{topic}` | ");
        let row = source
            .lines()
            .find(|l| l.starts_with(&needle) && !l.contains("`spot-"))
            .unwrap_or_else(|| panic!("{topic} has no row in the null table"));
        let cell = row[needle.len()..].trim_end_matches(" |");
        let optional = optional_fields(topic);
        if optional.is_empty() {
            assert!(cell.starts_with("none"), "{topic}: {cell}");
        } else {
            let stated: Vec<String> = cell
                .split('`')
                .skip(1)
                .step_by(2)
                .map(str::to_owned)
                .collect();
            let mut sorted = stated.clone();
            sorted.sort();
            assert_eq!(sorted, optional, "{topic}: the table names other fields");
        }
    }
}

#[test]
fn null_is_accepted_for_every_optional_field_and_refused_for_every_required_one() {
    for topic in NINE {
        let schema = system::schema_of(topic).unwrap_or_else(|| panic!("{topic}"));
        let base = example(topic);
        for (name, field) in &schema.fields {
            let data = with(&base, name, Some(Json::Null));
            let result = schema.validate(&data);
            if field.required {
                assert!(
                    result.is_err(),
                    "{topic}.{name}: null accepted for a required field"
                );
            } else {
                assert_eq!(
                    result,
                    Ok(()),
                    "{topic}.{name}: null refused for an optional field"
                );
            }
        }
    }
}

#[test]
fn user_notice_rules() {
    let t = "sys_user_notice";
    let base = example(t);
    bad(t, &with(&base, "user", Some(json!("not-an-id"))));
    bad(t, &with(&base, "user", None));
    bad(t, &with(&base, "payload", None));
    bad(t, &with(&base, "payload", Some(json!("text"))));
    bad(t, &with(&base, "payload", Some(json!([1]))));
    ok(t, &with(&base, "payload", Some(json!({}))));
    ok(
        t,
        &with(
            &base,
            "payload",
            Some(json!({"n": 3, "ok": true, "x": null, "a": ["p", "q"]})),
        ),
    );
    // The realtime boundary: no nested object, no array in an array, no float.
    bad(t, &with(&base, "payload", Some(json!({"o": {"a": 1}}))));
    bad(t, &with(&base, "payload", Some(json!({"a": [[1]]}))));
    bad(t, &with(&base, "payload", Some(json!({"a": [{"b": 1}]}))));
    bad(t, &with(&base, "payload", Some(json!({"f": 0.5}))));
    // Size: 2 KiB of compact JSON at most, strings 256 bytes at most.
    let many: serde_json::Map<String, Json> = (0..80)
        .map(|i| (format!("k{i:03}"), json!("x".repeat(30))))
        .collect();
    bad(t, &with(&base, "payload", Some(Json::Object(many))));
    ok(
        t,
        &with(&base, "payload", Some(json!({"s": "y".repeat(256)}))),
    );
    bad(
        t,
        &with(&base, "payload", Some(json!({"s": "y".repeat(257)}))),
    );
}

fn drop_key(v: &mut Json, key: &str) {
    if let Some(m) = v.as_object_mut() {
        m.remove(key);
    }
}

fn effect(base: &Json) -> Json {
    base["effects"][0].clone()
}

#[test]
fn operation_receipt_rules() {
    let t = "sys_operation_receipt";
    let base = example(t);
    for op in ["correct", "Post", ""] {
        bad(t, &with(&base, "operation", Some(json!(op))));
    }
    for op in sp::RECEIPT_OPERATIONS {
        ok(t, &with(&base, "operation", Some(json!(op))));
    }
    bad(t, &with(&base, "name", Some(json!("SINV-0001"))));
    bad(t, &with(&base, "doctype", Some(json!("Sales Invoice"))));
    bad(t, &with(&base, "version", Some(json!(-1))));
    bad(
        t,
        &with(&base, "committed_at", Some(json!("2026-11-02T00:00:00Z"))),
    );
    // The event exists only when an effect was reported.
    bad(t, &with(&base, "effects", Some(json!([]))));
    ok(t, &with(&base, "undeclared", None));
    // Indexes into effects: in range and distinct.
    ok(t, &with(&base, "undeclared", Some(json!([0, 1]))));
    bad(t, &with(&base, "undeclared", Some(json!([2]))));
    bad(t, &with(&base, "undeclared", Some(json!([1, 1]))));
    bad(t, &with(&base, "undeclared", Some(json!([-1]))));
    // The bounds of one receipt.
    let e = effect(&base);
    bad(
        t,
        &with(
            &base,
            "effects",
            Some(json!(vec![e.clone(); sp::MAX_EFFECTS_PER_RECEIPT + 1])),
        ),
    );
    let mut heavy = e.clone();
    heavy["store_refs"] = json!(vec!["r"; sp::MAX_REFS_PER_RECEIPT / 2 + 1]);
    bad(
        t,
        &with(&base, "effects", Some(json!([heavy.clone(), heavy]))),
    );
}

#[test]
fn effect_entry_rules_hold_in_both_effect_topics() {
    let receipt = example("sys_operation_receipt");
    let announce = example("sys_operation_receipt_effect");
    let cases: [(&str, Json); 12] = [
        ("step", json!("check_credit")),
        ("step", json!("erp:Step")),
        ("class", json!("Balance")),
        ("kind", json!("remote")),
        ("count", json!(-1)),
        ("count", json!(4_294_967_296_i64)),
        ("digest", json!("ABC")),
        ("digest", json!("a".repeat(63))),
        ("store_refs", json!([""])),
        ("store_refs", json!(["x".repeat(129)])),
        ("store_refs", json!(["a\nb"])),
        ("state", json!("exploded")),
    ];
    for (field, value) in cases {
        let mut e = effect(&receipt);
        e[field] = value.clone();
        bad(
            "sys_operation_receipt",
            &with(&receipt, "effects", Some(json!([e.clone()]))),
        );
        bad(
            "sys_operation_receipt_effect",
            &with(&announce, "effect", Some(e)),
        );
    }
    for state in [
        json!("applied"),
        json!("held"),
        json!("released"),
        json!("delivered"),
        json!({"failed": {"code": "mail.refused"}}),
    ] {
        let mut e = effect(&receipt);
        e["state"] = state;
        ok(
            "sys_operation_receipt_effect",
            &with(&announce, "effect", Some(e)),
        );
    }
    for state in [
        json!({"failed": {"code": "Refused"}}),
        json!({"failed": {"code": ""}}),
        json!({"failed": {"code": "x".repeat(65)}}),
        json!({"failed": {}}),
        json!({"failed": {"code": "a", "text": "b"}}),
        json!({"delivered": {}}),
        json!(null),
    ] {
        let mut e = effect(&receipt);
        e["state"] = state;
        bad(
            "sys_operation_receipt_effect",
            &with(&announce, "effect", Some(e)),
        );
    }
    bad(
        "sys_operation_receipt_effect",
        &with(&announce, "undeclared", Some(json!(0))),
    );
    bad(
        "sys_operation_receipt_effect",
        &with(&announce, "operation", Some(json!("correct"))),
    );
}

#[test]
fn partition_range_low_rules() {
    let t = "sys_partition_range_low";
    let base = example(t);
    ok(t, &with(&base, "node", Some(json!("hub.example.com"))));
    for node in ["Hub", "hub#", "hub#Branch", "-hub", "hub..com", "", "a b"] {
        bad(t, &with(&base, "node", Some(json!(node))));
    }
    bad(t, &with(&base, "series", Some(json!(""))));
    bad(t, &with(&base, "series", Some(json!("x".repeat(141)))));
    bad(t, &with(&base, "series", Some(json!("a\tb"))));
    ok(t, &with(&base, "series", Some(json!("SINV-2026-"))));
    // Counts are integers, never negative, and the remainder is within the lease.
    bad(t, &with(&base, "remaining", Some(json!(-1))));
    bad(t, &with(&base, "remaining", Some(json!(1001))));
    bad(t, &with(&base, "remaining", Some(json!(1.5))));
    bad(t, &with(&base, "remaining", Some(json!("50"))));
    ok(t, &with(&base, "remaining", Some(json!(0))));
}

#[test]
fn bpm_governance_rules() {
    let t = "sys_bpm_governance";
    let exception = example(t);
    let change = example("sys_bpm_governance.policy_change");
    bad(
        t,
        &with(&exception, "kind", Some(json!("bpm.governance.other"))),
    );
    for actor in [
        "root",
        "agent:7",
        "user:",
        "user:x",
        "User:0192f1c0-0000-7000-8000-000000000043",
    ] {
        bad(t, &with(&exception, "actor", Some(json!(actor))));
    }
    ok(t, &with(&exception, "actor", Some(json!("system"))));
    ok(
        t,
        &with(
            &exception,
            "actor",
            Some(json!("consumer:0192f1c0-0000-7000-8000-000000000043")),
        ),
    );
    ok(
        t,
        &with(
            &exception,
            "on_behalf_of",
            Some(json!("user:0192f1c0-0000-7000-8000-000000000044")),
        ),
    );
    bad(t, &with(&exception, "on_behalf_of", Some(json!("system"))));
    bad(t, &with(&exception, "masked", Some(json!(false))));
    // The payload is tagged values, only the keys the producer writes, and text only.
    bad(
        t,
        &with(&exception, "payload", Some(json!({"company": "Acme"}))),
    );
    let mut p = exception["payload"].clone();
    p["extra"] = json!({"Text": "x"});
    bad(t, &with(&exception, "payload", Some(p)));
    let mut p = exception["payload"].clone();
    p["reason"] = json!({"Int": 3});
    bad(t, &with(&exception, "payload", Some(p)));
    let mut p = exception["payload"].clone();
    p["mode"] = json!({"Text": "anything"});
    bad(t, &with(&exception, "payload", Some(p)));
    let mut p = exception["payload"].clone();
    p["policy"] = json!({"Text": "other"});
    bad(t, &with(&exception, "payload", Some(p)));
    // What each kind carries.
    let mut p = exception["payload"].clone();
    drop_key(&mut p, "reason");
    bad(t, &with(&exception, "payload", Some(p)));
    bad(t, &with(&exception, "subject", None));
    let mut refused = exception.clone();
    refused["kind"] = json!("bpm.governance.refused");
    bad(t, &refused);
    let mut p = exception["payload"].clone();
    if let Some(m) = p.as_object_mut() {
        m.remove("reason");
        m.insert(
            "detail".to_owned(),
            json!({"Text": "created outside its request"}),
        );
        m.insert("mode".to_owned(), json!({"Text": "process_only"}));
    }
    refused["payload"] = p;
    ok(t, &refused);
    // A policy change names a record type and no record.
    bad(
        t,
        &with(&change, "subject", Some(exception["subject"].clone())),
    );
    let mut p = change["payload"].clone();
    drop_key(&mut p, "doctype");
    bad(t, &with(&change, "payload", Some(p)));
    // The subject's record type is the payload's.
    let mut other = exception.clone();
    other["subject"]["doctype"] = json!("purchase_invoice");
    bad(t, &other);
}

#[test]
fn bpm_case_migrated_rules() {
    let t = "sys_bpm_case_migrated";
    let base = example(t);
    ok(t, &with(&base, "actor", None));
    bad(t, &with(&base, "actor", Some(json!(""))));
    for case in ["Case1", "1case", "", "case-1"] {
        bad(t, &with(&base, "case", Some(json!(case))));
    }
    bad(t, &with(&base, "plan_digest", Some(json!("abc"))));
    for side in ["from", "to"] {
        let mut d = base.clone();
        d[side]["hash"] = json!("ABC");
        bad(t, &d);
        let mut d = base.clone();
        d[side]["version"] = json!(-1);
        bad(t, &d);
        let mut d = base.clone();
        d[side]["name"] = json!("");
        bad(t, &d);
        let mut d = base.clone();
        d[side]["extra"] = json!(1);
        bad(t, &d);
    }
    for field in ["store_version", "nodes_changed", "variables_defaulted"] {
        bad(t, &with(&base, field, Some(json!(-1))));
        bad(t, &with(&base, field, Some(json!(1.5))));
    }
}

#[test]
fn condition_rules() {
    let raised = "sys_condition_raised";
    let cleared = "sys_condition_cleared";
    let r = example(raised);
    let e = example("sys_condition_raised.escalated");
    let c = example(cleared);
    for id in [
        "lag_high",
        "LAG__HIGH",
        "_LAG",
        "LAG_",
        "1LAG",
        "",
        "LAG HIGH",
    ] {
        bad(raised, &with(&r, "id", Some(json!(id))));
    }
    bad(raised, &with(&r, "id", Some(json!("A".repeat(65)))));
    ok(raised, &with(&r, "id", Some(json!("A".repeat(64)))));
    bad(raised, &with(&r, "severity", Some(json!("green"))));
    bad(raised, &with(&r, "at_millis", Some(json!(-1))));
    bad(raised, &with(&r, "at_millis", Some(json!(1.5))));
    // Params are text only.
    bad(raised, &with(&r, "params", Some(json!({"n": 3}))));
    bad(raised, &with(&r, "params", Some(json!("x"))));
    ok(raised, &with(&r, "params", Some(json!({}))));
    // A raise carries a severity; an escalation `from` and `to`, rising.
    bad(raised, &with(&r, "severity", None));
    bad(raised, &with(&r, "from", Some(json!("info"))));
    bad(raised, &with(&e, "severity", Some(json!("red"))));
    bad(raised, &with(&e, "from", None));
    bad(raised, &with(&e, "to", Some(json!("amber"))));
    bad(raised, &with(&e, "to", Some(json!("info"))));
    ok(raised, &with(&e, "from", Some(json!("info"))));
    // The cleared topic is for the clear only, and always carries its severity.
    bad(raised, &with(&r, "transition", Some(json!("cleared"))));
    bad(cleared, &with(&c, "transition", Some(json!("raised"))));
    bad(cleared, &with(&c, "transition", Some(json!("escalated"))));
    bad(cleared, &with(&c, "severity", None));
    bad(cleared, &with(&c, "from", Some(json!("info"))));
}

#[test]
fn the_free_form_list_names_only_what_the_nine_need() {
    // Every free-form name the nine rely on is listed (the schema walk in `tests.rs` fails on an
    // unlisted one), and none looks like a secret.
    let added = [
        "job",
        "args",
        "idempotency_key",
        "not_before",
        "payload",
        "name",
        "step",
        "class",
        "store_refs",
        "digest",
        "state",
        "series",
        "subject",
        "company",
        "policy",
        "reason",
        "detail",
        "case",
        "hash",
        "plan_digest",
        "id",
        "params",
    ];
    for name in added {
        assert!(system::FREE_FORM_FIELDS.contains(&name), "{name}");
        assert!(
            !identity::FORBIDDEN_KEY_PARTS
                .iter()
                .any(|p| name.contains(p)),
            "{name}"
        );
    }
}
