//! The checker must stay efficient: a large serial history is checked quickly, and a
//! 100,000-transaction history is checked within a stated bound.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::{Duration, Instant};

use spot_l7_invariants::history::{Level, Outcome};
use spot_l8_isolation::checker::check;
use spot_l8_isolation::history::{Observed, TxnEntry, TxnHistory, TxnOp, Workload};

/// The bound this test promises the checker holds to for a 100,000-transaction history.
const BOUND: Duration = Duration::from_secs(60);

fn read_reg(key: &str, v: &str) -> TxnOp {
    TxnOp::Read {
        key: key.to_owned(),
        observed: Observed::Register(Some(v.to_owned())),
    }
}

/// A serial history: `txns` transactions, each reading the value the previous writer of its key
/// left and writing a unique next value. A serial executor cannot violate any level, so the
/// checker must find nothing, at any level.
fn serial_history(txns: u64, keys: usize) -> TxnHistory {
    let mut history = TxnHistory::new(1, 1, "perf", Workload::RwRegister, keys as u32);
    let mut current = vec!["0".to_owned(); keys];
    let mut entries = Vec::with_capacity(txns as usize);
    for id in 1..=txns {
        let k = (id as usize) % keys;
        let key = format!("k{k}");
        let value = format!("v{id}");
        let previous = std::mem::replace(&mut current[k], value.clone());
        entries.push(TxnEntry {
            txn: id,
            worker: 0,
            seq: id,
            start_us: id * 10,
            end_us: id * 10 + 5,
            isolation: Level::ReadCommitted,
            outcome: Outcome::Committed,
            attempts: 1,
            ops: vec![read_reg(&key, &previous), TxnOp::Write { key, value }],
        });
    }
    history.entries = entries;
    history
}

#[test]
fn a_serial_executor_is_clean_and_a_hundred_thousand_transactions_check_in_seconds() {
    // No false positives: 10,000 serial transactions, at every level.
    let small = serial_history(10_000, 64);
    for level in [
        Level::ReadCommitted,
        Level::RepeatableRead,
        Level::Serializable,
    ] {
        let report = check(&small, level);
        assert!(
            report.ok(),
            "a serial 10,000-transaction history must be clean at {level:?}: {:?}",
            report.violations
        );
    }

    // Scale: 100,000 transactions, within the bound, with the time printed.
    let large = serial_history(100_000, 2_000);
    let start = Instant::now();
    let report = check(&large, Level::Serializable);
    let elapsed = start.elapsed();
    println!(
        "checked {} transactions ({} entries) in {elapsed:?}",
        report.transactions,
        large.entries.len(),
    );
    assert!(
        report.ok(),
        "a serial history must be clean: {:?}",
        report.violations
    );
    assert!(
        elapsed < BOUND,
        "checking 100,000 transactions took {elapsed:?}, over the {BOUND:?} bound"
    );
}
