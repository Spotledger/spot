//! Per-request cost accounting.
//!
//! A [`RequestCost`] lives in a `tokio` task-local for the duration of [`scope`], so it follows
//! a request across `.await` points on whichever worker thread polls it. It does **not** follow
//! `tokio::spawn`: work spawned from a request counts toward no request.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::task::futures::TaskLocalFuture;

use crate::metrics;

/// What one request or job cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestCost {
    /// Database statements run.
    pub db_statements: u64,
    /// Time spent in database statements.
    pub db_time: Duration,
    /// Rows read.
    pub rows_read: u64,
    /// Rows written.
    pub rows_written: u64,
    /// Cache lookups that hit.
    pub cache_hits: u64,
    /// Cache lookups that missed.
    pub cache_misses: u64,
    /// Outbox events written.
    pub outbox_events: u64,
    /// Calls to external services.
    pub external_calls: u64,
    /// Wall time of the whole scope.
    pub wall_time: Duration,
}

#[derive(Default)]
pub(crate) struct CostCell {
    db_statements: AtomicU64,
    db_time_ns: AtomicU64,
    rows_read: AtomicU64,
    rows_written: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    outbox_events: AtomicU64,
    external_calls: AtomicU64,
}

fn add(counter: &AtomicU64, n: u64) {
    // Saturating, so a runaway counter cannot wrap to a small number.
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        Some(v.saturating_add(n))
    });
}

impl CostCell {
    pub(crate) fn snapshot(&self, wall_time: Duration) -> RequestCost {
        RequestCost {
            db_statements: self.db_statements.load(Ordering::Relaxed),
            db_time: Duration::from_nanos(self.db_time_ns.load(Ordering::Relaxed)),
            rows_read: self.rows_read.load(Ordering::Relaxed),
            rows_written: self.rows_written.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            outbox_events: self.outbox_events.load(Ordering::Relaxed),
            external_calls: self.external_calls.load(Ordering::Relaxed),
            wall_time,
        }
    }
}

tokio::task_local! {
    static COST: Arc<CostCell>;
}

pub(crate) fn run_in<F: Future>(cell: Arc<CostCell>, fut: F) -> TaskLocalFuture<Arc<CostCell>, F> {
    COST.scope(cell, fut)
}

fn with_cell(f: impl FnOnce(&CostCell)) {
    // `try_with` fails outside a scope: recording is then a no-op.
    let _ = COST.try_with(|cell| f(cell));
}

/// Runs `fut` with a fresh [`RequestCost`] and returns its output with the final cost. Use it
/// for background jobs; the HTTP layer does the same per request. Scopes nest: the inner
/// scope's cost is separate and is not added to the outer one.
pub async fn scope<F: Future>(fut: F) -> (F::Output, RequestCost) {
    let cell = Arc::new(CostCell::default());
    let start = Instant::now();
    let output = run_in(Arc::clone(&cell), fut).await;
    (output, cell.snapshot(start.elapsed()))
}

/// The cost so far of the current scope (wall time is zero until the scope ends), or `None`
/// outside a scope.
pub fn current() -> Option<RequestCost> {
    COST.try_with(|cell| cell.snapshot(Duration::ZERO)).ok()
}

/// Records one database statement. The `spot_db_*` metrics are updated even outside a scope;
/// the per-request counters only inside one.
pub fn record_db(duration: Duration, rows_read: u64, rows_written: u64) {
    metrics::record_db_statement(duration);
    with_cell(|cell| {
        add(&cell.db_statements, 1);
        add(
            &cell.db_time_ns,
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX),
        );
        add(&cell.rows_read, rows_read);
        add(&cell.rows_written, rows_written);
    });
}

/// Records one cache lookup. No-op outside a scope.
pub fn record_cache(hit: bool) {
    with_cell(|cell| {
        add(
            if hit {
                &cell.cache_hits
            } else {
                &cell.cache_misses
            },
            1,
        );
    });
}

/// Records `count` outbox events written. No-op outside a scope.
pub fn record_outbox_events(count: u64) {
    with_cell(|cell| add(&cell.outbox_events, count));
}

/// Records one call to an external service. No-op outside a scope.
pub fn record_external_call() {
    with_cell(|cell| add(&cell.external_calls, 1));
}
