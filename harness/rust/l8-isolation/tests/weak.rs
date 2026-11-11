//! The weakened-backend proofs: each wrapper breaks one rule, and the checker must name the
//! matching anomaly with a witness.
//!
//! Each test also checks the wrapper's classification with a **hand-built history first**: a
//! minimal history exhibiting the class, so a checker bug cannot hide behind a wrapper bug.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;

use spot_l7_invariants::history::{Level, Outcome};
use spot_l8_isolation::anomaly::AnomalyClass;
use spot_l8_isolation::backends::run_level_weak;
use spot_l8_isolation::checker::check;
use spot_l8_isolation::history::{Observed, TxnEntry, TxnHistory, TxnOp, Workload};
use spot_l8_isolation::model::Model;
use spot_l8_isolation::weak::Weakness;

fn txn(id: u64, ops: Vec<TxnOp>) -> TxnEntry {
    TxnEntry {
        txn: id,
        worker: 0,
        seq: id,
        start_us: 10,
        end_us: 20,
        isolation: Level::ReadCommitted,
        outcome: Outcome::Committed,
        attempts: 1,
        ops,
    }
}

fn read_reg(key: &str, v: &str) -> TxnOp {
    TxnOp::Read {
        key: key.to_owned(),
        observed: Observed::Register(Some(v.to_owned())),
    }
}

fn write(key: &str, v: &str) -> TxnOp {
    TxnOp::Write {
        key: key.to_owned(),
        value: v.to_owned(),
    }
}

fn reg_history(entries: Vec<TxnEntry>) -> TxnHistory {
    let mut h = TxnHistory::new(0, 1, "hand", Workload::RwRegister, 2);
    h.entries = entries;
    h
}

/// The hand-built history that must be classified as `class` before the wrapper is trusted.
fn assert_checker_sees(class: AnomalyClass, history: &TxnHistory, level: Level) {
    let report = check(history, level);
    assert!(
        report.count(class) > 0,
        "the checker must classify the hand-built history as {}; saw {:?}",
        class.name(),
        report.observed()
    );
}

async fn run(
    weakness: Weakness,
    level: Level,
    seed: u64,
) -> spot_l8_isolation::report::IsolationReport {
    let config = common::conflict_heavy(Workload::RwRegister, seed);
    run_level_weak(Arc::new(Model::default()), &config, level, weakness)
        .await
        .unwrap()
}

fn violating(
    report: &spot_l8_isolation::report::IsolationReport,
    classes: &[AnomalyClass],
) -> bool {
    report.violations.iter().any(|c| classes.contains(c))
}

/// Runs the wrapper until it shows one of `classes`, over a bounded set of seeds. The anomaly a
/// wrapper causes depends on how the concurrent interleaving falls, so a single seed can miss it;
/// the test is still deterministic in the sense that it fails only if *no* seed shows it.
async fn run_until(
    weakness: Weakness,
    level: Level,
    classes: &[AnomalyClass],
) -> spot_l8_isolation::report::IsolationReport {
    let mut last = None;
    for seed in 1..=8 {
        let report = run(weakness, level, seed).await;
        if violating(&report, classes) {
            return report;
        }
        last = Some(report);
    }
    last.unwrap_or_else(|| panic!("no report"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dirty_read_is_reported() {
    // Hand-built first: a reader of an aborted writer's value.
    let h = reg_history(vec![
        TxnEntry {
            outcome: Outcome::RolledBack,
            ..txn(1, vec![write("k0", "X")])
        },
        txn(2, vec![read_reg("k0", "X")]),
    ]);
    assert_checker_sees(AnomalyClass::G1a, &h, Level::ReadCommitted);

    let report = run_until(
        Weakness::DirtyRead,
        Level::ReadCommitted,
        &[
            AnomalyClass::G1a,
            AnomalyClass::G1b,
            AnomalyClass::G1c,
            AnomalyClass::G0,
        ],
    )
    .await;
    assert!(
        violating(
            &report,
            &[
                AnomalyClass::G1a,
                AnomalyClass::G1b,
                AnomalyClass::G1c,
                AnomalyClass::G0
            ]
        ),
        "dirty reads must be caught: {:?}",
        report.violations
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_lock_lost_update_is_reported() {
    let h = reg_history(vec![
        txn(1, vec![read_reg("k0", "0"), write("k0", "A")]),
        txn(2, vec![read_reg("k0", "0"), write("k0", "B")]),
    ]);
    assert_checker_sees(AnomalyClass::LostUpdate, &h, Level::RepeatableRead);

    // The brief names both a lost update and G0 for this weakening.
    let report = run_until(
        Weakness::NoLockLostUpdate,
        Level::RepeatableRead,
        &[AnomalyClass::LostUpdate, AnomalyClass::G0],
    )
    .await;
    assert!(
        violating(&report, &[AnomalyClass::LostUpdate, AnomalyClass::G0]),
        "a losing write must be caught: {:?}",
        report.violations
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_per_statement_is_reported() {
    // Hand-built read skew: T2 updates both keys, T1 sees the old k0 and the new k1.
    let h = reg_history(vec![
        txn(2, vec![write("k0", "A"), write("k1", "B")]),
        txn(1, vec![read_reg("k0", "0"), read_reg("k1", "B")]),
    ]);
    assert_checker_sees(AnomalyClass::GSingle, &h, Level::RepeatableRead);

    let report = run_until(
        Weakness::SnapshotPerStatement,
        Level::RepeatableRead,
        &[AnomalyClass::GSingle, AnomalyClass::NonRepeatableRead],
    )
    .await;
    assert!(
        violating(
            &report,
            &[AnomalyClass::GSingle, AnomalyClass::NonRepeatableRead]
        ),
        "read skew must be caught: {:?}",
        report.violations
    );
    // Refreshing the snapshot weakens repeatability, not atomicity or dirty reads.
    assert!(
        !violating(
            &report,
            &[
                AnomalyClass::G0,
                AnomalyClass::G1a,
                AnomalyClass::G1b,
                AnomalyClass::G1c
            ]
        ),
        "a per-statement snapshot must not cause dirty-read anomalies: {:?}",
        report.violations
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serializable_no_validation_is_reported() {
    let h = reg_history(vec![
        txn(
            1,
            vec![read_reg("k0", "0"), read_reg("k1", "0"), write("k0", "A")],
        ),
        txn(
            2,
            vec![read_reg("k0", "0"), read_reg("k1", "0"), write("k1", "B")],
        ),
    ]);
    assert_checker_sees(AnomalyClass::G2, &h, Level::Serializable);

    let report = run_until(
        Weakness::SerializableNoValidation,
        Level::Serializable,
        &[AnomalyClass::G2],
    )
    .await;
    assert!(
        violating(&report, &[AnomalyClass::G2]),
        "write skew must be caught: {:?}",
        report.violations
    );
    // Missing serialisation validation permits write skew, not dirty reads.
    assert!(
        !violating(
            &report,
            &[
                AnomalyClass::G0,
                AnomalyClass::G1a,
                AnomalyClass::G1b,
                AnomalyClass::G1c
            ]
        ),
        "no serialisation validation must not cause dirty-read anomalies: {:?}",
        report.violations
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_replica_is_reported() {
    // Hand-built stale read: a committed append the read missed.
    let mut h = TxnHistory::new(0, 1, "hand", Workload::ListAppend, 2);
    h.entries = vec![
        TxnEntry {
            txn: 2,
            worker: 0,
            seq: 0,
            start_us: 5,
            end_us: 6,
            isolation: Level::ReadCommitted,
            outcome: Outcome::Committed,
            attempts: 1,
            ops: vec![TxnOp::Append {
                key: "k0".to_owned(),
                value: "X".to_owned(),
            }],
        },
        TxnEntry {
            txn: 1,
            worker: 0,
            seq: 0,
            start_us: 10,
            end_us: 20,
            isolation: Level::ReadCommitted,
            outcome: Outcome::Committed,
            attempts: 1,
            ops: vec![TxnOp::Read {
                key: "k0".to_owned(),
                observed: Observed::List(vec![]),
            }],
        },
    ];
    assert_checker_sees(AnomalyClass::StaleRead, &h, Level::ReadCommitted);

    let report = run_until(
        Weakness::StaleReplica,
        Level::ReadCommitted,
        &[AnomalyClass::StaleRead, AnomalyClass::G1b],
    )
    .await;
    assert!(
        violating(&report, &[AnomalyClass::StaleRead, AnomalyClass::G1b]),
        "a stale read must be caught: {:?}",
        report.violations
    );
}
