//! The JSON form is pinned, and carries only ids.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::{
    ConditionId, ConditionParams, HealthThresholds, Millis, Open, Severity, Subject,
};
use spot_sync_health::{NodeHealthFacts, NodeStatus, build};

const HOUR: u64 = 3_600_000;
const GOLDEN: &str = include_str!("golden/health_view.json");

fn open(id: &str, subject: &str, severity: Severity, raised: u64, params: &[(&str, &str)]) -> Open {
    let mut p = ConditionParams::new();
    for (k, v) in params {
        p.set(k, *v);
    }
    Open {
        id: ConditionId::parse(id).expect("id"),
        subject: Subject::new(subject),
        severity,
        params: p,
        raised_at_millis: Millis::new(raised),
        last_seen_millis: Millis::new(raised),
    }
}

fn view_json() -> String {
    let mut open_set = vec![
        open(
            "NODE_TOO_OLD",
            "node:branch-12",
            Severity::Red,
            50 * HOUR,
            &[
                ("node", "branch-12"),
                ("node_version", "1.1.0"),
                ("minimum_version", "1.3.0"),
            ],
        ),
        open(
            "MISSING_DEP",
            "record:inv-1",
            Severity::Amber,
            10 * HOUR,
            &[("node", "branch-3"), ("record", "inv-1")],
        ),
        open(
            "MISSING_DEP",
            "record:inv-2",
            Severity::Amber,
            11 * HOUR,
            &[("node", "branch-3"), ("record", "inv-2")],
        ),
        open(
            "CERT_EXPIRING",
            "node:branch-3",
            Severity::Amber,
            12 * HOUR,
            &[("node", "branch-3")],
        ),
    ];
    for n in 0..20 {
        open_set.push(open(
            "DRIFT_REPAIRED",
            &format!("record:{n}"),
            Severity::Info,
            90 * HOUR,
            &[],
        ));
    }
    let nodes = [
        NodeHealthFacts {
            node: "branch-12".to_owned(),
            last_contact: Millis::new(60 * HOUR),
            applied_prefix: 800,
            head: 1000,
            apply_rate_per_minute: 20,
            version: "1.1.0".to_owned(),
            status: NodeStatus::TooOld,
            as_of: Millis::new(60 * HOUR),
        },
        NodeHealthFacts {
            node: "branch-3".to_owned(),
            last_contact: Millis::new(99 * HOUR),
            applied_prefix: 1000,
            head: 1000,
            apply_rate_per_minute: 0,
            version: "1.4.2".to_owned(),
            status: NodeStatus::Connected,
            as_of: Millis::new(99 * HOUR),
        },
    ];
    let view = build(
        &open_set,
        &nodes,
        Millis::new(100 * HOUR),
        &HealthThresholds::new(Millis::new(4 * HOUR)),
    );
    view.to_json().expect("json")
}

#[test]
fn the_json_form_is_pinned() {
    if std::env::var_os("SPOT_UPDATE_GOLDEN").is_some() {
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/health_view.json"),
            view_json() + "\n",
        )
        .expect("write");
        return;
    }
    assert_eq!(view_json().trim_end(), GOLDEN.trim_end());
}

fn keys(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map {
                out.push(key.clone());
                keys(inner, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| keys(item, out)),
        _ => {}
    }
}

#[test]
fn the_json_has_no_payload_looking_field_and_every_sample_is_an_id() {
    let value: serde_json::Value = serde_json::from_str(&view_json()).expect("parse");
    let mut all = Vec::new();
    keys(&value, &mut all);
    for key in &all {
        for banned in [
            "payload",
            "body",
            "content",
            "data",
            "name_text",
            "address",
            "amount",
        ] {
            assert!(!key.contains(banned), "payload-looking field {key}");
        }
    }
    let rows = value["needs_you"].as_array().expect("rows");
    assert!(!rows.is_empty());
    for row in rows {
        for subject in row["subjects_sample"].as_array().expect("sample") {
            let text = subject.as_str().expect("text");
            assert!(
                !text.is_empty()
                    && text
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_-.:/#+".contains(c)),
                "subject is not an id: {text}"
            );
        }
    }
}
