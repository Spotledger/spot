//! The health view: calm by default, grouped, stable, bounded, id-only.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::{
    ConditionId, ConditionParams, HealthThresholds, Millis, Open, Severity, Subject,
};
use spot_sync_acks::NodeReportRow;
use spot_sync_health::{
    Colour, HealthError, MAX_ROWS, NodeHealthFacts, NodeStatus, REDACTED, Route, SAMPLE_SIZE,
    Status, build,
};

const HOUR: u64 = 3_600_000;

fn thresholds() -> HealthThresholds {
    HealthThresholds::new(Millis::new(4 * HOUR))
}

fn open(id: &str, subject: &str, severity: Severity, raised: u64) -> Open {
    Open {
        id: ConditionId::parse(id).expect("id"),
        subject: Subject::new(subject),
        severity,
        params: ConditionParams::new().with("node", subject),
        raised_at_millis: Millis::new(raised),
        last_seen_millis: Millis::new(raised),
    }
}

fn facts(node: &str, prefix: u64, status: NodeStatus) -> NodeHealthFacts {
    NodeHealthFacts {
        node: node.to_owned(),
        last_contact: Millis::new(100 * HOUR),
        applied_prefix: prefix,
        head: 1000,
        apply_rate_per_minute: 10,
        version: "1.4.2".to_owned(),
        status,
        as_of: Millis::new(100 * HOUR),
    }
}

#[test]
fn a_thousand_info_conditions_are_calm_and_list_nothing() {
    let infos: Vec<Open> = (0..1000)
        .map(|n| open("DRIFT_REPAIRED", &format!("record:{n}"), Severity::Info, 1))
        .collect();
    let view = build(&infos, &[], Millis::new(1000 * HOUR), &thresholds());
    assert_eq!(view.status, Status::Calm);
    assert_eq!(view.headline_key, "sync.health.headline.calm");
    assert!(view.needs_you.is_empty());
    assert_eq!(view.counts.info, 1000);
    assert_eq!(view.counts.needs_you_items, 0);
    assert!(!view.truncated);
}

#[test]
fn a_red_condition_needs_a_person_at_once() {
    let red = [open("NODE_TOO_OLD", "node:12", Severity::Red, 99 * HOUR)];
    let view = build(&red, &[], Millis::new(99 * HOUR + 1), &thresholds());
    assert_eq!(view.status, Status::Action);
    assert_eq!(view.needs_you.len(), 1);
    assert_eq!(view.needs_you[0].remedy_key, "sync.remedy.offer_upgrade");
    assert_eq!(view.needs_you[0].link, Route::Node);
}

#[test]
fn two_too_old_nodes_collapse_into_one_row_that_offers_the_upgrade() {
    let red = [
        open("NODE_TOO_OLD", "node:12", Severity::Red, 99 * HOUR),
        open("NODE_TOO_OLD", "node:13", Severity::Red, 98 * HOUR),
    ];
    let view = build(&red, &[], Millis::new(100 * HOUR), &thresholds());
    assert_eq!(view.needs_you.len(), 1);
    assert_eq!(view.needs_you[0].count, 2);
    assert_eq!(view.needs_you[0].remedy_key, "sync.remedy.offer_upgrade");
}

#[test]
fn an_entry_without_a_primary_remedy_keeps_the_first_operator_command() {
    let red = [open("NODE_SILENT", "node:12", Severity::Red, 99 * HOUR)];
    let view = build(&red, &[], Millis::new(100 * HOUR), &thresholds());
    assert_eq!(view.needs_you.len(), 1);
    assert_eq!(view.needs_you[0].remedy_key, "sync.operator.notify_contact");
}

#[test]
fn a_young_amber_is_calm_and_an_aged_one_needs_you() {
    let amber = [open("MISSING_DEP", "record:a", Severity::Amber, 100 * HOUR)];
    let young = build(&amber, &[], Millis::new(101 * HOUR), &thresholds());
    assert_eq!(young.status, Status::Calm);
    assert_eq!(young.counts.amber, 1);
    let aged = build(&amber, &[], Millis::new(105 * HOUR), &thresholds());
    assert_eq!(aged.status, Status::Attention);
    assert_eq!(aged.needs_you.len(), 1);
    assert_eq!(aged.needs_you[0].link, Route::Record);
}

#[test]
fn an_amber_with_no_operator_command_is_counted_never_listed() {
    // CERT_EXPIRING renews itself and names no operator command.
    let amber = [open("CERT_EXPIRING", "node:3", Severity::Amber, 0)];
    let view = build(&amber, &[], Millis::new(1000 * HOUR), &thresholds());
    assert_eq!(view.status, Status::Calm);
    assert!(view.needs_you.is_empty());
    assert_eq!(view.counts.amber, 1);
}

#[test]
fn same_remedy_collapses_with_a_count_and_a_sample_of_five() {
    let items: Vec<Open> = (0..12)
        .map(|n| {
            open(
                "MISSING_DEP",
                &format!("record:{n:02}"),
                Severity::Amber,
                (10 - n.min(10)) * HOUR,
            )
        })
        .collect();
    let view = build(&items, &[], Millis::new(500 * HOUR), &thresholds());
    assert_eq!(view.needs_you.len(), 1);
    let row = &view.needs_you[0];
    assert_eq!(row.count, 12);
    assert_eq!(row.subjects_sample.len(), SAMPLE_SIZE);
    assert_eq!(row.more, 7);
    // Oldest first: record:10 and record:11 were raised at 0, record:09 at 1h.
    assert_eq!(row.subjects_sample[0], "record:10");
    assert_eq!(row.since, Millis::ZERO);
}

#[test]
fn rows_sort_by_severity_then_oldest_and_never_reorder() {
    let items = vec![
        open("MISSING_DEP", "record:a", Severity::Amber, 10 * HOUR),
        open("NODE_SILENT", "node:1", Severity::Red, 50 * HOUR),
        open("HEAL_BUDGET_EXHAUSTED", "node:2", Severity::Red, 20 * HOUR),
        open("LAG_HIGH", "node:3", Severity::Amber, 5 * HOUR),
    ];
    let now = Millis::new(100 * HOUR);
    let forward = build(&items, &[], now, &thresholds());
    let mut reversed_items = items.clone();
    reversed_items.reverse();
    let backward = build(&reversed_items, &[], now, &thresholds());
    assert_eq!(forward, backward);
    assert_eq!(
        forward.to_json().expect("json"),
        backward.to_json().expect("json")
    );
    let severities: Vec<Severity> = forward.needs_you.iter().map(|row| row.severity).collect();
    assert_eq!(
        severities,
        [
            Severity::Red,
            Severity::Red,
            Severity::Amber,
            Severity::Amber
        ]
    );
    assert!(forward.needs_you[0].since <= forward.needs_you[1].since);
    assert!(forward.needs_you[2].since <= forward.needs_you[3].since);
}

#[test]
fn node_rows_carry_lag_estimate_colour_and_as_of() {
    let nodes = [
        facts("b", 1000, NodeStatus::Connected),
        facts("a", 400, NodeStatus::Lagging),
        facts("c", 1000, NodeStatus::Connected),
    ];
    let open = [open("OBLIGATION_RED", "node:c", Severity::Red, 99 * HOUR)];
    let view = build(&open, &nodes, Millis::new(101 * HOUR), &thresholds());
    let rows: Vec<(&str, Colour)> = view
        .nodes
        .iter()
        .map(|r| (r.node.as_str(), r.colour))
        .collect();
    assert_eq!(
        rows,
        [
            ("c", Colour::Red),
            ("a", Colour::Amber),
            ("b", Colour::Green)
        ]
    );
    let a = &view.nodes[1];
    assert_eq!(a.lag_positions, 600);
    assert_eq!(a.lag_estimate, Some(Millis::new(600 * 60_000 / 10)));
    assert_eq!(a.silent_for, Millis::new(HOUR));
    assert_eq!(a.as_of, Millis::new(100 * HOUR));
    assert_eq!(a.version, "1.4.2");
}

#[test]
fn an_unknown_rate_gives_no_estimate_and_a_node_ahead_of_the_head_has_no_lag() {
    let mut ahead = facts("a", 2000, NodeStatus::Connected);
    ahead.apply_rate_per_minute = 0;
    let view = build(&[], &[ahead], Millis::new(101 * HOUR), &thresholds());
    assert_eq!(view.nodes[0].lag_positions, 0);
    assert_eq!(view.nodes[0].lag_estimate, None);
}

#[test]
fn a_node_given_twice_keeps_its_newest_facts() {
    let old = facts("a", 100, NodeStatus::Lagging);
    let mut new = facts("a", 900, NodeStatus::Connected);
    new.as_of = Millis::new(101 * HOUR);
    let view = build(&[], &[old, new], Millis::new(102 * HOUR), &thresholds());
    assert_eq!(view.nodes.len(), 1);
    assert_eq!(view.nodes[0].lag_positions, 100);
}

#[test]
fn both_lists_are_capped_and_the_view_says_so() {
    let nodes: Vec<NodeHealthFacts> = (0..MAX_ROWS + 50)
        .map(|n| facts(&format!("n{n:04}"), 1000, NodeStatus::Connected))
        .collect();
    let view = build(&[], &nodes, Millis::new(101 * HOUR), &thresholds());
    assert_eq!(view.nodes.len(), MAX_ROWS);
    assert_eq!(view.counts.nodes, MAX_ROWS + 50);
    assert!(view.truncated);
    let exact = &nodes[..MAX_ROWS];
    assert!(!build(&[], exact, Millis::new(101 * HOUR), &thresholds()).truncated);
}

#[test]
fn text_that_is_not_an_id_is_never_shown() {
    let mut bad = open(
        "NODE_TOO_OLD",
        "Invoice 4411 for Acme Ltd",
        Severity::Red,
        0,
    );
    bad.params.set("node", "Acme Ltd, 12 High St");
    let view = build(&[bad], &[], Millis::new(HOUR), &thresholds());
    assert_eq!(view.needs_you[0].subjects_sample, [REDACTED]);
    assert_eq!(view.needs_you[0].params.get("node"), Some(REDACTED));
}

#[test]
fn a_negative_contact_time_is_refused_with_its_code() {
    let row: NodeReportRow = serde_json::from_value(serde_json::json!({
        "node": "n1",
        "last_contact_millis": -1,
        "applied_prefix": 5,
        "held": false,
        "version": "1.0.0",
        "level": "Ok",
    }))
    .expect("row");
    let error = NodeHealthFacts::from_row(&row, 10, 1, NodeStatus::Connected, Millis::ZERO)
        .expect_err("negative");
    assert_eq!(
        error,
        HealthError::NegativeContactTime {
            node: "n1".to_owned()
        }
    );
    assert_eq!(error.code(), "sync_health.negative_contact_time");

    let good: NodeReportRow = serde_json::from_value(serde_json::json!({
        "node": "n1", "last_contact_millis": 7, "applied_prefix": 5,
        "held": false, "version": "1.0.0", "level": "Ok",
    }))
    .expect("row");
    let facts = NodeHealthFacts::from_row(&good, 10, 1, NodeStatus::Connected, Millis::new(9))
        .expect("facts");
    assert_eq!(facts.last_contact, Millis::new(7));
    assert_eq!(facts.lag_positions(), 5);
}
