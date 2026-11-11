//! Policy tests over a fake connection source: no database needed.
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use spot_postgres::{PgBackend, PgConfig, Tls};

use crate::Bridge;
use crate::config::BridgeConfig;
use crate::core::{Abandon, BoxFuture, Checked, Core, Failure, Source};
use crate::error::BridgeError;

#[derive(Default)]
struct Counters {
    checkouts: AtomicUsize,
    abandons: AtomicUsize,
}

struct Fake {
    counters: Arc<Counters>,
    checkout_delay: Duration,
    fail: Option<fn() -> Failure>,
}

struct FakeAbandon(Arc<Counters>);

impl Abandon for FakeAbandon {
    fn abandon(self: Box<Self>) -> BoxFuture<'static, ()> {
        self.0.abandons.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

impl Source for Fake {
    type Conn = u32;

    fn checkout(&self) -> BoxFuture<'static, Result<Checked<u32>, Failure>> {
        let counters = Arc::clone(&self.counters);
        let delay = self.checkout_delay;
        let fail = self.fail;
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if let Some(make) = fail {
                return Err(make());
            }
            counters.checkouts.fetch_add(1, Ordering::SeqCst);
            Ok(Checked {
                conn: 7,
                abandon: Box::new(FakeAbandon(counters)),
            })
        })
    }
}

fn cfg(name: &str, in_flight: usize, queue: usize) -> BridgeConfig {
    BridgeConfig {
        name: name.to_owned(),
        workers: 2,
        max_in_flight: in_flight,
        queue_capacity: queue,
        default_deadline: Duration::from_secs(5),
        max_deadline: Duration::from_secs(10),
    }
}

fn core(name: &str, in_flight: usize, queue: usize) -> (Arc<Core<Fake>>, Arc<Counters>) {
    let counters = Arc::new(Counters::default());
    let source = Fake {
        counters: Arc::clone(&counters),
        checkout_delay: Duration::ZERO,
        fail: None,
    };
    let core = Core::start(cfg(name, in_flight, queue), source).expect("start");
    (Arc::new(core), counters)
}

fn sleep_op(
    ms: u64,
) -> impl FnOnce(u32) -> BoxFuture<'static, Result<u32, String>> + Send + 'static {
    move |conn| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(conn)
        })
    }
}

#[test]
fn a_plain_call_succeeds_and_the_store_error_passes_through() {
    let (core, counters) = core("t_plain", 2, 4);
    assert_eq!(core.run(Duration::from_secs(1), sleep_op(1)), Ok(7));
    let failed = core.run(Duration::from_secs(1), |_conn| {
        Box::pin(async { Err::<u32, String>("store said no".to_owned()) })
    });
    assert_eq!(failed, Err(BridgeError::Store("store said no".to_owned())));
    assert_eq!(counters.abandons.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_from_a_multi_thread_runtime_is_wrong_thread() {
    let (core, counters) = core("t_wrong_mt", 2, 4);
    let result = core.run(Duration::from_secs(1), sleep_op(1));
    assert_eq!(result, Err(BridgeError::WrongThread));
    assert_eq!(counters.checkouts.load(Ordering::SeqCst), 0);
    assert_eq!(
        BridgeError::<()>::WrongThread.code(),
        Some("pg_bridge.wrong_thread")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_from_a_current_thread_runtime_is_wrong_thread() {
    let (core, _) = core("t_wrong_ct", 2, 4);
    assert_eq!(
        core.run(Duration::from_secs(1), sleep_op(1)),
        Err(BridgeError::WrongThread)
    );
    assert_eq!(
        core.shutdown(Duration::from_secs(1)),
        Err(BridgeError::WrongThread)
    );
}

#[test]
fn a_full_queue_is_refused_at_once_and_nothing_is_queued() {
    let (core, counters) = core("t_busy", 1, 1);
    let mut holders = Vec::new();
    for _ in 0..2 {
        let core = Arc::clone(&core);
        holders.push(thread::spawn(move || {
            core.run(Duration::from_secs(5), sleep_op(400))
        }));
    }
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    let refused = core.run(Duration::from_secs(5), sleep_op(1));
    assert_eq!(refused, Err(BridgeError::Busy));
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "busy must be immediate"
    );
    for holder in holders {
        assert_eq!(holder.join().expect("join"), Ok(7));
    }
    // Only the two admitted calls ever took a connection.
    assert_eq!(counters.checkouts.load(Ordering::SeqCst), 2);
    assert!(BridgeError::<()>::Busy.is_retryable());
}

#[test]
fn a_late_call_times_out_cancels_and_the_bridge_recovers() {
    let (core, counters) = core("t_timeout", 2, 4);
    let started = Instant::now();
    let result = core.run(Duration::from_millis(100), sleep_op(5_000));
    assert_eq!(result, Err(BridgeError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(counters.abandons.load(Ordering::SeqCst), 1);
    assert_eq!(core.run(Duration::from_secs(1), sleep_op(1)), Ok(7));
}

#[test]
fn a_deadline_above_the_ceiling_is_clamped_not_refused() {
    let counters = Arc::new(Counters::default());
    let mut config = cfg("t_clamp", 2, 4);
    config.default_deadline = Duration::from_millis(50);
    config.max_deadline = Duration::from_millis(200);
    let source = Fake {
        counters,
        checkout_delay: Duration::ZERO,
        fail: None,
    };
    let core = Core::start(config, source).expect("start");
    let started = Instant::now();
    let result = core.run(Duration::from_secs(3_600), sleep_op(5_000));
    assert_eq!(result, Err(BridgeError::Timeout));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "clamped to the ceiling"
    );
}

#[test]
fn waiting_for_a_slot_counts_against_the_deadline() {
    let (core, _) = core("t_slot_wait", 1, 4);
    let holder = {
        let core = Arc::clone(&core);
        thread::spawn(move || core.run(Duration::from_secs(5), sleep_op(500)))
    };
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        core.run(Duration::from_millis(100), sleep_op(1)),
        Err(BridgeError::Timeout)
    );
    assert_eq!(holder.join().expect("join"), Ok(7));
}

#[test]
fn pool_failures_map_to_their_refusals() {
    type Case = (fn() -> Failure, BridgeError<String>);
    let cases: [Case; 3] = [
        (|| Failure::Timeout, BridgeError::Timeout),
        (|| Failure::Closed, BridgeError::Closed),
        (|| Failure::Connect, BridgeError::Connect),
    ];
    for (fail, expected) in cases {
        let source = Fake {
            counters: Arc::new(Counters::default()),
            checkout_delay: Duration::ZERO,
            fail: Some(fail),
        };
        let core = Core::start(cfg("t_fail", 2, 4), source).expect("start");
        assert_eq!(core.run(Duration::from_secs(1), sleep_op(1)), Err(expected));
    }
}

#[test]
fn overload_completes_or_refuses_cleanly_and_recovers() {
    let in_flight = 4;
    let (core, _) = core("t_overload", in_flight, 8);
    let callers = 10 * in_flight;
    let started = Instant::now();
    let handles: Vec<_> = (0..callers)
        .map(|_| {
            let core = Arc::clone(&core);
            thread::spawn(move || core.run(Duration::from_millis(600), sleep_op(50)))
        })
        .collect();
    let mut ok = 0;
    for handle in handles {
        match handle.join().expect("no caller hangs or panics") {
            Ok(_) => ok += 1,
            Err(BridgeError::Busy | BridgeError::Timeout) => {}
            Err(other) => panic!("unexpected refusal {other:?}"),
        }
    }
    assert!(ok >= in_flight, "work was done: {ok}");
    // Ideal: 40 calls of 50 ms over 4 slots is 500 ms. Bounded at 10x that, and by the deadline.
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(core.run(Duration::from_secs(5), sleep_op(1)), Ok(7));
}

#[test]
fn a_panicking_closure_is_closed_and_leaves_the_bridge_usable() {
    let (core, _) = core("t_panic", 2, 4);
    let result = core.run(
        Duration::from_secs(1),
        |_conn| -> BoxFuture<'static, Result<u32, String>> { panic!("closure defect") },
    );
    assert_eq!(result, Err(BridgeError::Closed));
    assert_eq!(core.run(Duration::from_secs(1), sleep_op(1)), Ok(7));
}

#[test]
fn shutdown_with_calls_in_flight_returns_within_grace_and_closes() {
    let (core, counters) = core("t_shutdown", 2, 4);
    let callers: Vec<_> = (0..2)
        .map(|_| {
            let core = Arc::clone(&core);
            thread::spawn(move || core.run(Duration::from_secs(10), sleep_op(5_000)))
        })
        .collect();
    thread::sleep(Duration::from_millis(150));
    let started = Instant::now();
    core.shutdown(Duration::from_millis(300)).expect("shutdown");
    assert!(started.elapsed() < Duration::from_millis(300) + Duration::from_secs(1));
    for caller in callers {
        assert_eq!(caller.join().expect("join"), Err(BridgeError::Closed));
    }
    assert_eq!(counters.abandons.load(Ordering::SeqCst), 2);
    assert_eq!(
        core.run(Duration::from_secs(1), sleep_op(1)),
        Err(BridgeError::Closed)
    );
}

#[test]
fn shutdown_lets_quick_calls_finish() {
    let (core, _) = core("t_shutdown_quick", 2, 4);
    let caller = {
        let core = Arc::clone(&core);
        thread::spawn(move || core.run(Duration::from_secs(5), sleep_op(200)))
    };
    thread::sleep(Duration::from_millis(50));
    core.shutdown(Duration::from_secs(3)).expect("shutdown");
    assert_eq!(caller.join().expect("join"), Ok(7));
}

#[test]
fn dropping_a_bridge_never_hangs_idle_or_after_a_timeout() {
    // The caller holds the only reference, so a drop cannot overlap a call; what must hold is
    // that `Drop` returns at once whether or not the runtime still has tasks winding down.
    let (busy, _) = core("t_drop", 2, 4);
    assert_eq!(
        busy.run(Duration::from_millis(80), sleep_op(5_000)),
        Err(BridgeError::Timeout)
    );
    let started = Instant::now();
    drop(busy);
    assert!(started.elapsed() < Duration::from_secs(1));
    let (idle, _) = core("t_drop_idle", 2, 4);
    let started = Instant::now();
    drop(idle);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn every_config_rule_is_a_constructor_error_naming_the_setting() {
    let pool = 10;
    let base = cfg("t_cfg", 4, 8);
    assert_eq!(base.validate(pool), Ok(()));
    let cases: Vec<(&str, BridgeConfig, usize)> = vec![
        (
            "name",
            BridgeConfig {
                name: String::new(),
                ..base.clone()
            },
            pool,
        ),
        (
            "name",
            BridgeConfig {
                name: "has space".to_owned(),
                ..base.clone()
            },
            pool,
        ),
        (
            "workers",
            BridgeConfig {
                workers: 1,
                ..base.clone()
            },
            pool,
        ),
        (
            "max_in_flight",
            BridgeConfig {
                max_in_flight: 0,
                ..base.clone()
            },
            pool,
        ),
        (
            "queue_capacity",
            BridgeConfig {
                queue_capacity: 0,
                ..base.clone()
            },
            pool,
        ),
        (
            "default_deadline",
            BridgeConfig {
                default_deadline: Duration::ZERO,
                ..base.clone()
            },
            pool,
        ),
        (
            "max_deadline",
            BridgeConfig {
                max_deadline: Duration::from_secs(1),
                ..base.clone()
            },
            pool,
        ),
        ("pool", base.clone(), 2),
        (
            "max_in_flight",
            BridgeConfig {
                max_in_flight: 9,
                ..base.clone()
            },
            pool,
        ),
        (
            "max_in_flight",
            BridgeConfig {
                max_in_flight: 8,
                ..base.clone()
            },
            8,
        ),
    ];
    for (setting, config, size) in cases {
        match config.validate(size) {
            Err(BridgeError::Config { setting: named, .. }) => assert_eq!(named, setting),
            other => panic!("{setting}: expected a config error, got {other:?}"),
        }
    }
    // The largest the rule allows is accepted: pool 10 allows 8.
    assert_eq!(BridgeConfig::largest_in_flight(10), 8);
    assert_eq!(
        BridgeConfig {
            max_in_flight: 8,
            ..base
        }
        .validate(10),
        Ok(())
    );
}

#[test]
fn metrics_move_as_scripted() {
    spot_telemetry::metrics::install().expect("recorder");
    let (core, _) = core("t_metrics", 1, 1);
    // Wrong thread is counted from a runtime thread.
    thread::scope(|scope| {
        scope.spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("rt");
            rt.block_on(async {
                let _ = core.run(Duration::from_secs(1), sleep_op(1));
            });
        });
    });
    // Two holders fill the slot and the queue; a third is busy.
    let holders: Vec<_> = (0..2)
        .map(|_| {
            let core = Arc::clone(&core);
            thread::spawn(move || core.run(Duration::from_secs(5), sleep_op(400)))
        })
        .collect();
    thread::sleep(Duration::from_millis(100));
    let during = spot_telemetry::render_prometheus();
    assert_eq!(value(&during, "spot_pg_bridge_in_flight"), Some(1.0));
    assert_eq!(value(&during, "spot_pg_bridge_queue_depth"), Some(1.0));
    assert_eq!(
        core.run(Duration::from_secs(1), sleep_op(1)),
        Err(BridgeError::Busy)
    );
    for holder in holders {
        assert_eq!(holder.join().expect("join"), Ok(7));
    }
    // A timeout cancels.
    assert_eq!(
        core.run(Duration::from_millis(80), sleep_op(3_000)),
        Err(BridgeError::Timeout)
    );
    let after = spot_telemetry::render_prometheus();
    assert_eq!(
        value(&after, "spot_pg_bridge_wrong_thread_total"),
        Some(1.0)
    );
    assert_eq!(value(&after, "spot_pg_bridge_busy_total"), Some(1.0));
    assert_eq!(value(&after, "spot_pg_bridge_timeout_total"), Some(1.0));
    assert_eq!(value(&after, "spot_pg_bridge_cancelled_total"), Some(1.0));
    assert_eq!(value(&after, "spot_pg_bridge_in_flight"), Some(0.0));
    assert_eq!(value(&after, "spot_pg_bridge_queue_depth"), Some(0.0));
    assert!(value(&after, "spot_pg_bridge_wait_seconds_count").unwrap_or(0.0) >= 2.0);
    assert!(value(&after, "spot_pg_bridge_run_seconds_count").unwrap_or(0.0) >= 2.0);
    assert!(!after.contains("SELECT"), "no SQL in the metrics");
}

/// The value of the sample `name{bridge="t_metrics"...}` in a Prometheus rendering.
fn value(text: &str, name: &str) -> Option<f64> {
    text.lines()
        .filter(|line| line.starts_with(name) && line.contains("bridge=\"t_metrics\""))
        .filter(|line| !line.contains("quantile"))
        .find_map(|line| line.rsplit(' ').next()?.parse().ok())
}

/// A backend whose config requires TLS, built without a server (`connect_unchecked` contacts
/// nothing), or `None` after one `SKIPPED:` line when this host has no platform root
/// certificates: loading those roots is the only thing that can fail here.
fn tls_backend() -> Option<PgBackend> {
    let mut config = PgConfig::new("127.0.0.1", 5432, "spot", "spot", "not-a-real-password");
    config.tls = Tls::Require;
    match PgBackend::connect_unchecked(config) {
        Ok(backend) => Some(backend),
        Err(error) => {
            assert!(
                error.explanation.contains("root certificates"),
                "a backend with Tls::Require failed for a reason other than the root store: \
                 {error}"
            );
            println!("SKIPPED: no platform root certificates to verify a TLS connection with");
            None
        }
    }
}

/// `for_backend` is the normal construction, so it must make both decisions the caller would
/// otherwise make by hand: the backend's pool (its size is what the config is checked against)
/// and the cancel transport that matches it — `NoTls` for a plain pool, the pool's own connector
/// for a TLS one. The bridge's stored `CancelWith` is asked; nothing connects and no key exists.
#[test]
fn for_backend_takes_the_backends_pool_and_matching_cancel_transport() {
    let plain_config = PgConfig::new("127.0.0.1", 5432, "spot", "spot", "not-a-real-password");
    let plain =
        PgBackend::connect_unchecked(plain_config).expect("a plain backend needs no server");
    assert!(
        plain.cancel_tls().is_none(),
        "the default config is plain, as this test assumes"
    );

    let bridge = Bridge::for_backend(cfg("t_for_backend_plain", 2, 4), &plain).expect("bridge");
    assert!(
        !bridge.cancel_uses_tls(),
        "a plain pool must cancel over NoTls, exactly as its connections travel"
    );

    // The pool that is validated is the backend's: 3 connections allow at most 1 in flight, so a
    // config for a larger pool is refused here. That is the proof `backend.pool()` was taken.
    let mut small_config = PgConfig::new("127.0.0.1", 5432, "spot", "spot", "not-a-real-password");
    small_config.pool_max_size = 3;
    let small = PgBackend::connect_unchecked(small_config).expect("a plain backend, pool of 3");
    match Bridge::for_backend(cfg("t_for_backend_small", 2, 4), &small) {
        Err(BridgeError::Config { setting, .. }) => assert_eq!(setting, "max_in_flight"),
        Err(other) => panic!("expected a config error, got {other:?}"),
        Ok(_) => panic!("a pool of 3 allows at most 1 in flight, so this must be refused"),
    }

    let Some(tls) = tls_backend() else { return };
    assert!(
        tls.cancel_tls().is_some(),
        "a TLS backend offers its connector, as this test assumes"
    );
    let bridge = Bridge::for_backend(cfg("t_for_backend_tls", 2, 4), &tls).expect("bridge");
    assert!(
        bridge.cancel_uses_tls(),
        "a TLS pool must send its cancel request over TLS: the key must not travel in clear"
    );
}
