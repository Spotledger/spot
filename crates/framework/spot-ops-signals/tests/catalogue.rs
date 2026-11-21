//! The naming and cardinality rules of the catalogue, and its agreement with the enums and with
//! `docs/running-spot/signals.md`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use spot_ops_signals::catalogue::{Kind, SERIES, Unit};
use spot_ops_signals::{conditions, journal, migration, sync};

const FORBIDDEN_LABELS: &[&str] = &[
    "site",
    "tenant",
    "node",
    "user",
    "doc",
    "document",
    "topic",
    "condition",
    "id",
    "name",
    "path",
];

#[test]
fn names_follow_the_rules() {
    let mut seen = BTreeSet::new();
    for s in SERIES {
        assert!(seen.insert(s.name), "{} is listed twice", s.name);
        assert!(s.name.starts_with("spot_"), "{}", s.name);
        assert!(
            s.name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{}",
            s.name
        );
        if s.kind == Kind::Counter {
            assert!(s.name.ends_with("_total"), "{} is a counter", s.name);
        } else {
            assert!(!s.name.ends_with("_total"), "{} is not a counter", s.name);
        }
        match s.unit {
            Unit::Seconds => assert!(s.name.ends_with("_seconds"), "{}", s.name),
            Unit::Count => {
                assert!(!s.name.ends_with("_seconds"), "{}", s.name);
                assert!(!s.name.ends_with("_bytes"), "{}", s.name);
            }
        }
        for l in s.labels {
            assert!(
                l.name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "label {} of {}",
                l.name,
                s.name
            );
            assert!(
                !FORBIDDEN_LABELS.contains(&l.name),
                "{} carries the forbidden label {}",
                s.name,
                l.name
            );
            assert!(!l.values.is_empty());
            let unique: BTreeSet<_> = l.values.iter().collect();
            assert_eq!(unique.len(), l.values.len(), "{}.{}", s.name, l.name);
        }
        assert!(!s.help.is_empty());
    }
}

fn values<T: Copy>(all: &[T], text: impl Fn(T) -> &'static str) -> Vec<&'static str> {
    all.iter().map(|v| text(*v)).collect()
}

fn catalogue_values(series: &str, label: &str) -> Vec<&'static str> {
    SERIES
        .iter()
        .find(|s| s.name == series)
        .unwrap()
        .labels
        .iter()
        .find(|l| l.name == label)
        .unwrap()
        .values
        .to_vec()
}

#[test]
fn the_catalogue_lists_exactly_the_enum_values() {
    let cases: Vec<(&str, &str, Vec<&str>)> = vec![
        (
            journal::SHIP_BATCHES_TOTAL,
            "outcome",
            values(journal::BatchOutcome::ALL, |v| v.as_str()),
        ),
        (
            journal::DURABILITY_DECISIONS_TOTAL,
            "decision",
            values(journal::Decision::ALL, |v| v.as_str()),
        ),
        (
            journal::AUDIT_RUNS_TOTAL,
            "result",
            values(journal::AuditResult::ALL, |v| v.as_str()),
        ),
        (
            sync::APPLY_TOTAL,
            "outcome",
            values(sync::ApplyOutcome::ALL, |v| v.as_str()),
        ),
        (
            sync::NODE_LAG_SECONDS,
            "stat",
            values(sync::LagStat::ALL, |v| v.as_str()),
        ),
        (
            sync::NODES,
            "state",
            values(sync::NodeState::ALL, |v| v.as_str()),
        ),
        (
            sync::OBLIGATIONS,
            "level",
            values(sync::ObligationLevel::ALL, |v| v.as_str()),
        ),
        (
            conditions::OPEN,
            "severity",
            values(conditions::Severity::ALL, |v| v.as_str()),
        ),
        (
            migration::VERBS_TOTAL,
            "verb",
            values(migration::Verb::ALL, |v| v.as_str()),
        ),
        (
            migration::VERBS_TOTAL,
            "outcome",
            values(migration::VerbOutcome::ALL, |v| v.as_str()),
        ),
    ];
    for (series, label, from_enum) in cases {
        assert_eq!(
            catalogue_values(series, label),
            from_enum,
            "{series}.{label}"
        );
    }
    // Every labelled series was covered above.
    let labelled: usize = SERIES.iter().map(|s| s.labels.len()).sum();
    assert_eq!(labelled, 10);
}

/// The rows of the documentation table: series name to its label names.
fn doc_rows() -> BTreeMap<String, Vec<String>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../docs/running-spot/signals.md");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut rows = BTreeMap::new();
    for line in text.lines().filter(|l| l.starts_with("| `spot_")) {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // | `name` | kind | unit | labels | meaning | alert |
        assert!(cells.len() >= 7, "a malformed row: {line}");
        let name = cells[1].trim_matches('`').to_owned();
        let labels: Vec<String> = cells[4]
            .split(',')
            .map(|l| l.trim().trim_matches('`').to_owned())
            .filter(|l| !l.is_empty() && l != "-")
            .collect();
        assert!(
            rows.insert(name.clone(), labels).is_none(),
            "{name} twice in the doc"
        );
    }
    rows
}

#[test]
fn the_doc_table_equals_the_catalogue() {
    let doc = doc_rows();
    let catalogue: BTreeMap<String, Vec<String>> = SERIES
        .iter()
        .map(|s| {
            (
                s.name.to_owned(),
                s.labels.iter().map(|l| l.name.to_owned()).collect(),
            )
        })
        .collect();
    assert_eq!(doc, catalogue, "signals.md and catalogue.rs disagree");
}

#[test]
fn the_catalogue_counts_what_the_task_declares() {
    assert_eq!(SERIES.len(), 19);
    let total: usize = SERIES.iter().map(|s| s.max_series()).sum();
    // 16 journal + 18 sync + 2 quarantine + 3 conditions + 22 migration
    assert_eq!(total, 61);
}
