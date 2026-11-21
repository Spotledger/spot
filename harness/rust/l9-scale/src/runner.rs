//! The open-loop runner.
//!
//! A dispatcher walks the [`Scheduler`] and hands every arrival to its session at the planned
//! time, whatever the server is doing. A session worker performs its arrivals one at a time,
//! as a person with one browser tab would, and measures each from the arrival's **intended**
//! start to its completion: when the server stalls, the arrivals queued behind the stalled
//! one are charged the wait, so the report cannot hide a stall by having slowed the load
//! (coordinated omission). The plain service time is recorded beside it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::mpsc;

use crate::clock::Clock;
use crate::metrics::{Class, Metrics, Outcome};
use crate::profile::{Op, Profile};
use crate::schedule::{Arrival, Scheduler};

/// A boxed future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Performs one operation for one session. The real implementation talks to the native API;
/// tests substitute a fake server.
pub trait Executor: Send + Sync + 'static {
    /// Performs `op` as simulated user `session`.
    fn execute(&self, session: u32, op: Op) -> BoxFuture<'_, Outcome>;
}

/// One reading of the server's resources.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ResourceSample {
    /// Seconds into the run.
    pub at_seconds: u64,
    /// Resident memory of the server process, in kilobytes.
    pub rss_kb: Option<u64>,
    /// Open file descriptors of the server process.
    pub open_fds: Option<u64>,
    /// Connections to the run's database (`pg_stat_activity`).
    pub pg_connections: Option<u64>,
    /// Lock requests waiting in the run's database (`pg_locks` not granted).
    pub pg_waiting_locks: Option<u64>,
}

/// Reads the server's resources; the launcher supplies one.
pub trait Sampler: Send + Sync + 'static {
    /// One reading (the runner stamps `at_seconds`).
    fn sample(&self) -> BoxFuture<'_, ResourceSample>;
}

/// What a run produced.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// The measurements.
    pub metrics: Metrics,
    /// Arrivals the schedule dropped because every session was thinking.
    pub shed: u64,
    /// Arrivals dispatched.
    pub dispatched: u64,
    /// Server resource readings.
    pub resources: Vec<ResourceSample>,
    /// Run length from the first arrival to the last completion, in nanoseconds.
    pub elapsed_ns: u64,
}

/// Runs `profile` against `executor`. `sampler` (when there is one) is read every
/// `sample_every_seconds`.
pub async fn run(
    profile: &Profile,
    executor: Arc<dyn Executor>,
    clock: Arc<dyn Clock>,
    sampler: Option<Arc<dyn Sampler>>,
) -> RunResult {
    let sessions = profile.sessions as usize;
    let total_ns = profile.total_ns();
    let timeout_ns = profile.request_timeout_ms.saturating_mul(1_000_000);
    // A queue that is still waiting this long after the end of the load is abandoned.
    let drain_deadline = total_ns.saturating_add(timeout_ns.saturating_mul(2));
    let start = clock.now_ns();

    let mut senders = Vec::with_capacity(sessions);
    let mut workers = Vec::with_capacity(sessions);
    for _ in 0..sessions {
        let (tx, rx) = mpsc::unbounded_channel::<Arrival>();
        senders.push(tx);
        workers.push(tokio::spawn(worker(
            rx,
            executor.clone(),
            clock.clone(),
            start,
            drain_deadline,
        )));
    }

    let readings = Arc::new(std::sync::Mutex::new(Vec::<ResourceSample>::new()));
    let sampler_task = sampler.map(|sampler| {
        let clock = clock.clone();
        let readings = readings.clone();
        let every = Duration::from_secs(profile.sample_every_seconds);
        SamplerGuard(Some(tokio::spawn(async move {
            loop {
                let mut reading = sampler.sample().await;
                reading.at_seconds = clock.now_ns().saturating_sub(start) / 1_000_000_000;
                if let Ok(mut all) = readings.lock() {
                    all.push(reading);
                }
                tokio::time::sleep(every).await;
            }
        })))
    });

    let mut scheduler = Scheduler::new(profile);
    let mut dispatched = 0u64;
    for arrival in scheduler.by_ref() {
        let now = clock.now_ns().saturating_sub(start);
        if arrival.at_ns > now {
            tokio::time::sleep(Duration::from_nanos(arrival.at_ns - now)).await;
        }
        if let Some(tx) = senders.get(arrival.session as usize)
            && tx.send(arrival).is_ok()
        {
            dispatched += 1;
        }
    }
    let shed = scheduler.shed();
    drop(senders);

    let mut metrics = Metrics::new();
    for worker in workers {
        if let Ok(part) = worker.await {
            metrics.merge(&part);
        }
    }
    let elapsed_ns = clock.now_ns().saturating_sub(start);
    drop(sampler_task);
    let resources = readings.lock().map(|r| r.clone()).unwrap_or_default();
    RunResult {
        metrics,
        shed,
        dispatched,
        resources,
        elapsed_ns,
    }
}

/// Stops the sampler task when dropped.
struct SamplerGuard(Option<tokio::task::JoinHandle<()>>);

impl Drop for SamplerGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

async fn worker(
    mut rx: mpsc::UnboundedReceiver<Arrival>,
    executor: Arc<dyn Executor>,
    clock: Arc<dyn Clock>,
    start: u64,
    drain_deadline: u64,
) -> Metrics {
    let mut metrics = Metrics::new();
    while let Some(arrival) = rx.recv().await {
        let began = clock.now_ns().saturating_sub(start);
        if began > drain_deadline {
            let done = clock.now_ns().saturating_sub(start);
            metrics.record(
                arrival.op,
                arrival.steady,
                Outcome::of(Class::Abandoned),
                done.saturating_sub(arrival.at_ns),
                0,
                done,
            );
            continue;
        }
        let outcome = executor.execute(arrival.session, arrival.op).await;
        let done = clock.now_ns().saturating_sub(start);
        metrics.record(
            arrival.op,
            arrival.steady,
            outcome,
            done.saturating_sub(arrival.at_ns),
            done.saturating_sub(began),
            done,
        );
    }
    metrics
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::clock::MonotonicClock;
    use crate::metrics::classify_status;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fake {
        calls: AtomicU64,
        first_stall_ms: u64,
        always_ms: u64,
    }

    impl Executor for Fake {
        fn execute(&self, _session: u32, _op: Op) -> BoxFuture<'_, Outcome> {
            Box::pin(async move {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                let wait = if n == 0 {
                    self.first_stall_ms
                } else {
                    self.always_ms
                };
                if wait > 0 {
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                }
                classify_status(200, false)
            })
        }
    }

    struct FixedSampler;

    impl Sampler for FixedSampler {
        fn sample(&self) -> BoxFuture<'_, ResourceSample> {
            Box::pin(async {
                ResourceSample {
                    rss_kb: Some(1234),
                    ..ResourceSample::default()
                }
            })
        }
    }

    fn profile(sessions: u32, steady: u64, tps: u32, timeout_ms: u64) -> Profile {
        Profile::from_toml(&format!(
            "name = \"r\"\ntier = \"x\"\nsessions = {sessions}\nsteady_seconds = {steady}\n\
             target_tps = {tps}\nrequest_timeout_ms = {timeout_ms}\nsample_every_seconds = 1\n\
             [think]\nkind = \"fixed\"\nms = 0\n[mix]\nopen_form = 1\n"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn a_stalled_server_inflates_the_reported_latency_but_not_the_service_time() {
        // One session, an arrival every 50 ms, and the first call stalls for 400 ms. A closed
        // loop would record one slow call and 19 fast ones; the open loop charges every
        // arrival that was due during the stall.
        let fake = Arc::new(Fake {
            calls: AtomicU64::new(0),
            first_stall_ms: 400,
            always_ms: 0,
        });
        let result = run(
            &profile(1, 1, 20, 5_000),
            fake,
            Arc::new(MonotonicClock::new()),
            None,
        )
        .await;
        let stats = result.metrics.op(Op::OpenForm);
        assert_eq!(result.dispatched, 20);
        assert_eq!(stats.total(), 20);
        assert!(stats.latency.max() >= 390_000, "{}", stats.latency.max());
        assert!(
            stats.latency.percentile(950) >= 300_000,
            "p95 {} should carry the stall",
            stats.latency.percentile(950)
        );
        assert!(
            stats.service.percentile(950) < 100_000,
            "the service time of the other calls is small: {}",
            stats.service.percentile(950)
        );
    }

    #[tokio::test]
    async fn a_healthy_run_completes_every_arrival_and_samples_resources() {
        let fake = Arc::new(Fake {
            calls: AtomicU64::new(0),
            first_stall_ms: 0,
            always_ms: 0,
        });
        let result = run(
            &profile(4, 2, 20, 5_000),
            fake,
            Arc::new(MonotonicClock::new()),
            Some(Arc::new(FixedSampler)),
        )
        .await;
        assert_eq!(result.dispatched, 40);
        assert_eq!(result.shed, 0);
        assert_eq!(result.metrics.all().total(), 40);
        assert_eq!(result.metrics.all().failures(), 0);
        assert!(result.metrics.op(Op::OpenForm).latency.percentile(990) < 100_000);
        assert!(result.resources.len() >= 2, "{:?}", result.resources);
        assert!(result.resources.iter().all(|r| r.rss_kb == Some(1234)));
        assert!(
            result
                .resources
                .windows(2)
                .all(|w| w[0].at_seconds <= w[1].at_seconds)
        );
        assert!(result.metrics.timeline.len() >= 2);
    }

    #[tokio::test]
    async fn work_still_queued_long_after_the_load_ended_is_abandoned_and_counted() {
        // 300 ms per call, 50 arrivals in one second on one session, a 100 ms timeout: the
        // backlog cannot drain within the grace period.
        let fake = Arc::new(Fake {
            calls: AtomicU64::new(0),
            first_stall_ms: 0,
            always_ms: 300,
        });
        let result = run(
            &profile(1, 1, 50, 100),
            fake,
            Arc::new(MonotonicClock::new()),
            None,
        )
        .await;
        let stats = result.metrics.op(Op::OpenForm);
        assert_eq!(stats.total(), 50);
        assert!(stats.abandoned > 0, "{stats:?}");
        assert!(stats.failures() >= stats.abandoned);
    }
}
