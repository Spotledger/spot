//! No double booking under contention: concurrent takers race each other and the sweeper over a
//! small set of units while a manual clock runs, and the final table holds no two overlapping
//! records on one unit. The exclusion constraint is the arbiter; this test looks for any gap in
//! the service around it (takeover, sweep, confirm) that would let two records overlap.
//!
//! `SPOT_STRESS_ATTEMPTS` sets the number of attempts (default 100000, the brief's 10^5).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_occupancy::{Caller, Limits, OccupancyError, Span};
use spot_storage_contract::{
    DocumentStore, Expr, Isolation, Query, QueryExecutor, QueryPath, RangeValue, RecordId, RowLock,
    Selection, StorageBackend, Transaction, TxOptions, Value,
};
use support::*;

const UNITS: usize = 50;
const WORKERS: u64 = 32;

/// A small deterministic generator: no clock, no entropy source, the run is repeatable.
struct Lcg(u64);

impl Lcg {
    fn step(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

#[derive(Default)]
struct Tally {
    ok: u64,
    conflicts: u64,
    retries_exhausted: u64,
    confirms: u64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn no_double_booking_across_concurrent_takers_racing_the_sweeper() {
    let attempts: u64 = std::env::var("SPOT_STRESS_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let Some(fx) = fixture(UNITS, Span::Dates, Limits::default()).await else {
        return;
    };
    let units = Arc::new(fx.units.clone());
    let done = Arc::new(AtomicBool::new(false));

    // The sweeper and the clock: time moves two minutes per tick (a hold lives five ticks), and
    // every tick sweeps, racing the takers.
    let sweeper = {
        let (svc, clock, done) = (fx.svc.clone(), fx.clock.clone(), done.clone());
        tokio::spawn(async move {
            let mut busy_sweeps = 0u64;
            while !done.load(Ordering::Relaxed) {
                clock.advance(secs(120));
                match svc.sweep_all(5).await {
                    Ok(_) => {}
                    // A sweep refused for load (see `busy`) is skipped: the next tick sweeps
                    // again, and a missed sweep can only leave a hold standing longer.
                    Err(OccupancyError::Storage(e)) if busy(&e) => busy_sweeps += 1,
                    Err(e) => panic!("sweep failed unexpectedly: {e}"),
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            busy_sweeps
        })
    };

    let per_worker = attempts / WORKERS + 1;
    let mut workers = Vec::new();
    for w in 0..WORKERS {
        let (svc, units) = (fx.svc.clone(), units.clone());
        workers.push(tokio::spawn(async move {
            let mut rng = Lcg(0x9E37_79B9_7F4A_7C15 ^ (w + 1));
            let mut tally = Tally::default();
            let who = format!("w{w}");
            for _ in 0..per_worker {
                let r = rng.step();
                let unit = units[(r % UNITS as u64) as usize];
                let from = 1 + ((r >> 8) % 24) as i64;
                let to = from + 1 + ((r >> 16) % 4) as i64;
                let request = claim(unit, from, to, &who);
                let result = if (r >> 24).is_multiple_of(4) {
                    svc.book_now(&request).await
                } else {
                    svc.hold(&request).await
                };
                match result {
                    Ok(h) => {
                        tally.ok += 1;
                        // Half the holds are confirmed (and may lose to the sweeper).
                        if h.expires_at.is_some() && (r >> 28).is_multiple_of(2) {
                            match svc.confirm(h.id, &Caller::Holder(holder(&who))).await {
                                Ok(_) => tally.confirms += 1,
                                Err(OccupancyError::Expired(_)) => {}
                                Err(OccupancyError::Storage(e)) if busy(&e) => {}
                                Err(e) => panic!("confirm failed unexpectedly: {e}"),
                            }
                        }
                    }
                    Err(OccupancyError::Conflict(_)) => tally.conflicts += 1,
                    Err(OccupancyError::Storage(e)) if busy(&e) => {
                        tally.retries_exhausted += 1;
                    }
                    Err(e) => panic!("claim failed unexpectedly: {e}"),
                }
            }
            tally
        }));
    }
    let mut total = Tally::default();
    for w in workers {
        let t = w.await.unwrap();
        total.ok += t.ok;
        total.conflicts += t.conflicts;
        total.retries_exhausted += t.retries_exhausted;
        total.confirms += t.confirms;
    }
    done.store(true, Ordering::Relaxed);
    let busy_sweeps = sweeper.await.unwrap();
    println!(
        "stress: attempts~{} ok={} conflicts={} retries_exhausted={} confirms={} busy_sweeps={busy_sweeps}",
        per_worker * WORKERS,
        total.ok,
        total.conflicts,
        total.retries_exhausted,
        total.confirms
    );
    assert!(
        total.ok > 0 && total.conflicts > 0,
        "the run did not contend"
    );

    // The final table: no two records overlap on one unit. The query compiler cannot select a
    // range column yet (ADR-087, WP-STO-02.T9), so select the ids and load each record.
    let mut q = Query::from(&n("occupancy"), &n("o"), QueryPath::Transactional);
    q.select = vec![Selection {
        expr: Expr::field(&n("o"), &n("id")),
        alias: n("id"),
    }];
    let mut tx = fx
        .backend
        .begin(
            fx.catalog.clone(),
            TxOptions::new(n("occupancy_stress_check"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let rows = tx.query(&q).await.unwrap();
    let mut by_unit: std::collections::BTreeMap<RecordId, Vec<RangeValue>> = Default::default();
    for row in &rows.rows {
        let Value::Id(id) = &row[0] else {
            panic!("unexpected row shape");
        };
        let doc = tx
            .load(&n("occupancy"), *id, RowLock::None)
            .await
            .unwrap()
            .expect("a listed record loads");
        let (Some(Value::Id(unit)), Some(Value::Range(r))) = (
            doc.data.fields.get(&n("unit")),
            doc.data.fields.get(&n("during")),
        ) else {
            panic!("unexpected record shape");
        };
        by_unit.entry(*unit).or_default().push(*r);
    }
    tx.rollback().await.unwrap();
    let mut checked = 0u64;
    for ranges in by_unit.values() {
        for (i, a) in ranges.iter().enumerate() {
            for b in &ranges[i + 1..] {
                assert!(!a.overlaps(b), "double booking: {a:?} and {b:?}");
                checked += 1;
            }
        }
    }
    println!("stress: {} rows, {checked} pairs checked", rows.rows.len());
}

/// A storage refusal a loaded system may legitimately answer: retries exhausted, the bounded
/// wait on a row lock (`storage.lock_timeout`) ran out, or a statement outlived its 30 s bound
/// (`storage.statement_timeout`: 32 workers, the sweeper, autovacuum and the server share two
/// vCPUs on a hosted CI runner, in a debug build). None can leave a double booking: a refused
/// statement commits nothing, and the overlap check on the final table below is the assertion.
fn busy(e: &spot_storage_contract::StorageError) -> bool {
    e.kind.is_retryable()
        || matches!(
            e.kind,
            spot_storage_contract::ErrorKind::LockTimeout
                | spot_storage_contract::ErrorKind::StatementTimeout
        )
}
