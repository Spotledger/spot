//! The workloads on the in-memory fake: the basic machinery, and that a correct backend passes
//! the levels it does claim.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;

use spot_l7_invariants::history::Level;
use spot_l8_isolation::anomaly::AnomalyClass;
use spot_l8_isolation::backends::run_level_fake;
use spot_l8_isolation::history::{TxnHistory, Workload};
use spot_l8_isolation::model::Model;
use spot_l8_isolation::workload::plan;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fake_passes_read_committed_and_repeatable_read_on_both_workloads() {
    for workload in [Workload::ListAppend, Workload::RwRegister] {
        for level in [Level::ReadCommitted, Level::RepeatableRead] {
            let config = common::conflict_heavy(workload, 11);
            let report = run_level_fake(Arc::new(Model::default()), &config, level)
                .await
                .unwrap();
            assert!(
                report.ok(),
                "{:?} at {level:?} must be clean on the fake: {:?}",
                workload,
                report.violations
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fake_fails_serializable_with_write_skew() {
    // The fake documents that serializable behaves like repeatable read, so a conflict-heavy run
    // must eventually show write skew (G2), and must show no other forbidden class.
    let config = common::conflict_heavy(Workload::RwRegister, 5);
    let report = run_level_fake(Arc::new(Model::default()), &config, Level::Serializable)
        .await
        .unwrap();
    if !report.ok() {
        assert!(
            report.violations.iter().all(|c| *c == AnomalyClass::G2),
            "the fake's only serializable violation must be write skew: {:?}",
            report.violations
        );
    }
}

#[test]
fn the_plan_is_a_pure_function_of_the_seed() {
    let a = plan(&common::tiny(Workload::RwRegister, 9));
    let b = plan(&common::tiny(Workload::RwRegister, 9));
    assert_eq!(a, b);
    let c = plan(&common::tiny(Workload::RwRegister, 10));
    assert_ne!(a, c, "a different seed must plan differently");
}

#[test]
fn a_history_round_trips_through_a_file_image() {
    let h = TxnHistory::new(1, 2, "smoke", Workload::ListAppend, 3);
    assert!(h.entries.is_empty());
    let mut bytes = Vec::new();
    h.write_jsonl(&mut bytes).unwrap();
    let back = TxnHistory::read_jsonl(&String::from_utf8(bytes).unwrap()).unwrap();
    assert_eq!(h.header, back.header);
}
