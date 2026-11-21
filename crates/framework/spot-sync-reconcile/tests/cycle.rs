//! The acceptance test: a seeded missing document and an altered total are detected within one
//! reconciliation cycle, across 20 simulated edges.
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_storage_contract::{FeedPosition, Name, Version};
use spot_sync_reconcile::{
    Finding, MemorySource, ReconcileReport, RecordFacts, RecordKey, Scope,
    lag::{LagSample, ProducedRecord},
    measure_lag, plan_repairs, reconcile, reconcile_sources,
    repair::NodeRole,
};

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn key(entity: &str, record: &str, version: u64) -> RecordKey {
    RecordKey::new(name(entity), record, Version(version))
}

fn item(record: &str, qty: i128) -> RecordFacts {
    let mut facts = RecordFacts::of(key("item", record, 1));
    facts.totals = vec![(name("qty"), qty)];
    facts
}

fn invoice(record: &str, total: i128) -> RecordFacts {
    let mut facts = RecordFacts::of(key("invoice", record, 1));
    facts.totals = vec![(name("total"), total)];
    facts
}

fn gl(record: &str, account: &str, amount: i128) -> RecordFacts {
    let mut facts = RecordFacts::of(key("gl_entry", record, 1));
    facts.gl = vec![(account.to_owned(), amount)];
    facts
}

/// The owner's records: 40 items, 5 invoices and one GL entry.
fn owner_records() -> Vec<RecordFacts> {
    let mut records: Vec<RecordFacts> = (0..40).map(|i| item(&format!("item_{i:03}"), 5)).collect();
    records.extend((0..5).map(|i| invoice(&format!("INV-{i:03}"), 1_000)));
    records.push(gl("gl_1", "1100", 500));
    records
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drift {
    Missing,
    Extra,
    Version,
    Total,
}

impl Drift {
    fn expected_kind(self) -> &'static str {
        match self {
            Drift::Missing => "missing",
            Drift::Extra => "extra",
            Drift::Version => "version",
            Drift::Total => "total",
        }
    }
}

fn seeded(base: &[RecordFacts], drift: Drift, seed: usize) -> Vec<RecordFacts> {
    let mut records = base.to_vec();
    match drift {
        Drift::Missing => {
            records.remove(seed % records.len());
        }
        Drift::Extra => {
            records.push(item(&format!("ghost_{seed}"), 1));
        }
        Drift::Version => {
            let index = seed % records.len();
            records[index].key.version = Version(9);
        }
        Drift::Total => {
            let index = seed % records.len();
            records[index].totals = vec![(name("qty"), 999)];
        }
    }
    records
}

fn snapshot(scope: &Scope, records: Vec<RecordFacts>) -> spot_sync_reconcile::ScopeSnapshot {
    spot_sync_reconcile::ScopeSnapshot::new(scope.clone(), FeedPosition::new(1, 100), records)
}

#[test]
fn three_kinds_of_drift_across_twenty_edges_are_all_detected_in_one_cycle() {
    let scope = Scope::new("company", "C1");
    let base = owner_records();
    let owner = snapshot(&scope, base.clone());

    let drifts = [Drift::Missing, Drift::Extra, Drift::Version, Drift::Total];
    let mut detected = 0;
    for edge in 0..20usize {
        let drift = drifts[edge % drifts.len()];
        let mirror = snapshot(&scope, seeded(&base, drift, edge));
        let report: ReconcileReport = reconcile(&owner, &mirror, &scope);
        assert!(
            !report.of_kind(drift.expected_kind()).is_empty(),
            "edge {edge} ({drift:?}) was not detected in one cycle: {:?}",
            report.findings
        );
        detected += 1;
    }
    assert_eq!(detected, 20);
}

#[test]
fn a_clean_edge_reports_nothing_in_one_cycle() {
    let scope = Scope::new("company", "C1");
    let base = owner_records();
    let owner = snapshot(&scope, base.clone());
    let mirror = snapshot(&scope, base);
    let report = reconcile(&owner, &mirror, &scope);
    assert!(report.is_clean(), "{:?}", report.findings);
    assert_eq!(report.rounds, 0);
    assert_eq!(report.digest_bytes, 0);
}

#[test]
fn the_missing_document_and_the_altered_total_are_both_reported() {
    // The acceptance sentence, spelled out on one edge.
    let scope = Scope::new("company", "C1");
    let base = owner_records();
    let owner = snapshot(&scope, base.clone());

    let mut mirror_records = base.clone();
    mirror_records.remove(0); // a missing document
    let total_index = mirror_records
        .iter()
        .position(|facts| facts.key.entity == name("invoice"))
        .unwrap();
    mirror_records[total_index].totals = vec![(name("total"), 900)]; // an altered total

    let report = reconcile(&owner, &snapshot(&scope, mirror_records), &scope);
    assert!(
        !report.of_kind("missing").is_empty(),
        "{:?}",
        report.findings
    );
    assert!(!report.of_kind("total").is_empty(), "{:?}", report.findings);
}

#[test]
fn reconcile_sources_reads_both_nodes() {
    let scope = Scope::new("company", "C1");
    let base = owner_records();
    let owner = MemorySource::new(scope.clone(), FeedPosition::new(1, 10), base.clone());
    let mut mirror_records = base;
    mirror_records.remove(0);
    let mirror = MemorySource::new(scope.clone(), FeedPosition::new(1, 10), mirror_records);

    let report = support::block_on(reconcile_sources(&owner, &mirror, &scope)).unwrap();
    assert!(
        !report.of_kind("missing").is_empty(),
        "{:?}",
        report.findings
    );

    // The async read and the sync one agree.
    let sync_report = reconcile(
        &owner.snapshot_now(&scope),
        &mirror.snapshot_now(&scope),
        &scope,
    );
    assert_eq!(report.findings, sync_report.findings);
}

#[test]
fn findings_become_repairs_that_only_target_the_mirror() {
    let scope = Scope::new("company", "C1");
    let base = owner_records();
    let owner = snapshot(&scope, base.clone());
    let mut mirror_records = base;
    mirror_records.remove(0);
    mirror_records.push(item("ghost", 1));
    let report = reconcile(&owner, &snapshot(&scope, mirror_records), &scope);

    let repairs = plan_repairs(&report, NodeRole::Owner, NodeRole::Mirror, "branch").unwrap();
    assert!(repairs.iter().all(|repair| match repair {
        spot_sync_reconcile::Repair::Resend { destination, .. }
        | spot_sync_reconcile::Repair::RebuildMirror { destination, .. }
        | spot_sync_reconcile::Repair::RebuildScope { destination, .. } => destination == "branch",
    }));
    assert!(
        repairs
            .iter()
            .any(|repair| matches!(repair, spot_sync_reconcile::Repair::Resend { .. }))
    );
    assert!(
        repairs
            .iter()
            .any(|repair| matches!(repair, spot_sync_reconcile::Repair::RebuildMirror { .. }))
    );
}

#[test]
fn lag_is_measured_per_node_and_scope() {
    let now = time::OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap();
    let sample = LagSample::new("branch#1", 7)
        .for_scope(Scope::new("company", "C1"))
        .with_produced(vec![
            ProducedRecord::new(7, now - time::Duration::seconds(120)),
            ProducedRecord::new(8, now - time::Duration::seconds(30)),
            ProducedRecord::new(9, now - time::Duration::seconds(10)),
        ]);
    let lag = measure_lag(&sample, now);
    assert_eq!(lag.newest_produced, 9);
    assert_eq!(lag.lag, 2);
    assert_eq!(
        lag.oldest_unapplied_age,
        Some(std::time::Duration::from_secs(30))
    );
}

#[test]
fn every_finding_kind_maps_to_a_stable_label() {
    let findings = [
        Finding::MissingAtReceiver {
            entity: name("item"),
            name: "A".into(),
            version: Version(1),
        },
        Finding::ExtraAtReceiver {
            entity: name("item"),
            name: "B".into(),
            version: Version(1),
        },
        Finding::VersionMismatch {
            entity: name("item"),
            name: "C".into(),
            local: Version(2),
            remote: Version(1),
        },
        Finding::TotalMismatch {
            measure: spot_sync_reconcile::Measure::Count {
                entity: name("item"),
            },
            local: 2,
            remote: 1,
        },
    ];
    let kinds: Vec<&str> = findings.iter().map(Finding::kind).collect();
    assert_eq!(kinds, vec!["missing", "extra", "version", "total"]);
}
