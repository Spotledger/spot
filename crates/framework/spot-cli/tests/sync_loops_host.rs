//! The sync_loops module's registration facts (WP-RTB-02.T6); no database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_cli::services::node_role::{self, Role};
use spot_cli::services::sync_loops;

#[test]
fn the_module_is_edge_only_and_matches_the_role_table() {
    assert_eq!(sync_loops::ROLES, ["edge"]);
    assert!(node_role::service_set(Role::Edge).contains(&"sync_loops"));
    assert!(!node_role::service_set(Role::Hub).contains(&"sync_loops"));
    const { assert!(sync_loops::ORDER > node_role::ORDER) };
}

// ---------------------------------------------------------------------------------------------
// WP-RTB-02.T11: the assembly of the steps from their sources, the buffer, the health view.
// ---------------------------------------------------------------------------------------------

use std::sync::{Arc, Mutex};

use spot_cli::services::sync_loops::{
    Assembled, BufferSink, Op, QuarantineCache, Settings, Sources, assemble, tally,
};
use spot_sync_loops::certificate::{CertificateSource, Expiring};
use spot_sync_loops::closure::{RepairPull, RepairSource, Waiting};
use spot_sync_loops::freshness::{ClassFacts, FreshnessSource, StampSink};
use spot_sync_loops::healing::{Healer, HealingSource, Suspect};
use spot_sync_loops::quarantine::{OpenItem, QuarantineSource};
use spot_sync_loops::series::{Sample, SampleSource};
use spot_sync_loops::{MemoryDecisions, SharedDecisions, SyncHealthView};

struct Ages;
impl FreshnessSource for Ages {
    fn classes(&mut self, _max: usize) -> Result<Vec<ClassFacts>, String> {
        Ok(vec![ClassFacts {
            class: "prices".to_owned(),
            age_secs: 5_000,
        }])
    }
}

struct Stamps;
impl StampSink for Stamps {
    fn set_stale(&mut self, _class: &str, _stale: bool) {}
}

struct OneSuspect;
impl HealingSource for OneSuspect {
    fn suspects(&mut self, _max: usize) -> Result<Vec<Suspect>, String> {
        Ok(vec![Suspect {
            subject: "scope_a".to_owned(),
        }])
    }
}

struct Mends;
impl Healer for Mends {
    fn heal(&mut self, _subject: &str) -> Result<(), String> {
        Ok(())
    }
}

struct Waits;
impl RepairSource for Waits {
    fn waiting(&mut self, _max: usize) -> Result<Vec<Waiting>, String> {
        Ok(vec![Waiting {
            subject: "p10".to_owned(),
            age_secs: 10,
        }])
    }
}

#[derive(Clone, Default)]
struct Pulled(Arc<Mutex<Vec<String>>>);
impl RepairPull for Pulled {
    fn pull(&mut self, subject: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(subject.to_owned());
        Ok(())
    }
}

struct Certs;
impl CertificateSource for Certs {
    fn expiring(&mut self, _max: usize) -> Result<Vec<Expiring>, String> {
        Ok(vec![Expiring {
            node: "edge-khi#branch_01".to_owned(),
            days_left: 2,
        }])
    }
}

struct Numbers;
impl SampleSource for Numbers {
    fn samples(&mut self, _max: usize) -> Result<Vec<Sample>, String> {
        Ok(vec![Sample {
            name: "parked",
            value: 3.0,
        }])
    }
}

fn decisions() -> (SharedDecisions, MemoryDecisions) {
    let memory = MemoryDecisions::new();
    (SharedDecisions::new(Box::new(memory.clone())), memory)
}

#[test]
fn with_no_sources_only_reconcile_runs_and_it_says_it_is_not_available() {
    let (shared, _) = decisions();
    let view = SyncHealthView::new();
    let Assembled {
        mut loops,
        wired,
        not_wired,
    } = assemble(
        Sources::default(),
        &shared,
        &view,
        &Settings {
            reconcile_scopes: vec!["branch_01".to_owned()],
            ..Settings::default()
        },
        100,
    );
    assert_eq!(wired, ["reconcile"]);
    assert_eq!(
        not_wired,
        [
            "obligations",
            "liveness",
            "freshness",
            "healing",
            "closure_repair",
            "quarantine",
            "certificate",
            "series"
        ]
    );
    let mut sink = BufferSink::default();
    let report = loops.tick(1_000, &mut sink);
    assert_eq!(report.failed.len(), 1);
    assert!(report.failed[0].1.starts_with("not available"));
    // The summary stays calm: an information condition is not an alarm, and it is not a clean
    // result either: the failure is on the report and the view.
    view.update(1_000, report, loops.summary());
    assert_eq!(view.failed()[0].0, "reconcile");
}

#[test]
fn every_wired_step_runs_audits_its_responses_and_a_failure_stops_no_other() {
    let (shared, memory) = decisions();
    let view = SyncHealthView::new();
    let pulled = Pulled::default();
    let sources = Sources {
        freshness: Some((Box::new(Ages), Box::new(Stamps))),
        healing: Some((Box::new(OneSuspect), Box::new(Mends))),
        closure: Some((Box::new(Waits), Box::new(pulled.clone()))),
        certificates: Some(Box::new(Certs)),
        samples: Some(Box::new(Numbers)),
        ..Sources::default()
    };
    let Assembled {
        mut loops,
        wired,
        not_wired,
    } = assemble(
        sources,
        &shared,
        &view,
        &Settings {
            reconcile_scopes: vec!["branch_01".to_owned()],
            ..Settings::default()
        },
        100,
    );
    assert_eq!(
        wired,
        [
            "freshness",
            "reconcile",
            "healing",
            "closure_repair",
            "certificate",
            "series"
        ]
    );
    assert_eq!(not_wired, ["obligations", "liveness", "quarantine"]);

    let mut sink = BufferSink::default();
    let report = loops.tick(10_000, &mut sink);
    // Reconcile has no provider and fails; the five others ran regardless.
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.ran.len(), 5);
    assert_eq!(*pulled.0.lock().unwrap(), ["p10"]);
    let actions: Vec<(&str, &str)> = memory.all().iter().map(|d| (d.step, d.action)).collect();
    assert_eq!(
        actions,
        [
            ("freshness", "stamp_stale"),
            ("healing", "heal"),
            ("closure_repair", "repair_pull")
        ]
    );
    let raised = tally(&sink.0);
    assert_eq!(raised.get("amber"), Some(&1), "the stale class");
    assert_eq!(
        raised.get("red"),
        Some(&1),
        "the certificate inside three days"
    );
    assert_eq!(raised.get("info"), Some(&1), "reconcile is not available");
    assert!(
        sink.0
            .iter()
            .any(|op| matches!(op, Op::Raise(r) if r.condition == "CERT_EXPIRING"))
    );
    view.update(10_000, report, loops.summary());
    assert_eq!(view.line(), "sync_loops: 1 amber, 1 red");
    assert_eq!(view.series("parked"), [(10_000, 3.0)]);
}

#[test]
fn the_quarantine_cache_is_what_the_step_reads() {
    let cache = QuarantineCache::default();
    let mut source: Box<dyn QuarantineSource> = Box::new(cache.clone());
    assert!(source.open(10).unwrap().is_empty());
    cache.set(vec![OpenItem {
        subject: "7".to_owned(),
        age_secs: 100,
    }]);
    assert_eq!(source.open(10).unwrap().len(), 1);
}

#[test]
fn the_bound_is_checked() {
    assert_eq!(Settings::default().max_items().unwrap(), 200);
    for bad in [0, 10_001] {
        let settings = Settings {
            max_items: Some(bad),
            ..Settings::default()
        };
        assert!(settings.max_items().is_err());
    }
    let unknown: Result<Settings, _> = toml::from_str("speed = 1");
    assert!(unknown.is_err());
}

#[test]
fn the_hub_home_is_in_the_role_table() {
    assert_eq!(sync_loops::HUB_ROLES, ["hub"]);
    assert!(node_role::service_set(Role::Hub).contains(&sync_loops::HUB_NAME));
    assert!(!node_role::service_set(Role::Edge).contains(&sync_loops::HUB_NAME));
    const { assert!(sync_loops::HUB_ORDER > node_role::ORDER) };
}
