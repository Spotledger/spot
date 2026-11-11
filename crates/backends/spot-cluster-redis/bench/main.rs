//! The side-by-side benchmark the brief asks for: Redis/Valkey against the built-in backends.
//!
//! ```text
//! SPOT_TEST_REDIS_URL=redis://127.0.0.1:6379 \
//!   cargo run -p spot-cluster-redis --bin bench --features redis -- --ops 2000
//! ```
//!
//! It measures ops/s and p99 latency for cache `set`/`get`, lock acquire/release and rate-limit
//! `take`, on the Redis plugin and on the in-process cache and coordinator, and writes
//! `results.json`. Every number is integer arithmetic (no floats anywhere, per ADR-12), and no
//! measurement is invented: a run without a reachable server reports the built-in numbers only.
//!
//! The database-backed (PostgreSQL) coordinator is not measured here: it needs a live database,
//! which is the lead's CI service-container step, and the crate's bin cannot reach the
//! `[dev-dependencies]` that would build one. The profile doc records that.

use std::error::Error;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use spot_cache::{CacheStore, InProcessCache, InProcessConfig};
use spot_cluster_redis::{RedisCacheStore, RedisConfig, RedisConnection, RedisCoordinator};
use spot_coordinator::{
    Admit, Coordinator, InMemoryPresence, InProcessCoordinator, LockExt, NodeId, RateLimit,
};
use spot_core::{Clock, SiteId, SystemClock};

/// How many iterations each operation runs by default.
const DEFAULT_OPS: usize = 2000;

/// The default server URL when neither `--url` nor `SPOT_TEST_REDIS_URL` is set.
const DEFAULT_URL: &str = "redis://127.0.0.1:6379";

/// One operation's numbers.
struct Row {
    backend: &'static str,
    operation: &'static str,
    ops: usize,
    total_micros: u64,
    p99_micros: u64,
    mean_micros: u64,
}

impl Row {
    /// Integer ops per second (never a float).
    fn ops_per_sec(&self) -> u64 {
        if self.total_micros == 0 {
            return 0;
        }
        u64::try_from(self.ops)
            .unwrap_or(0)
            .saturating_mul(1_000_000)
            / self.total_micros
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "backend": self.backend,
            "operation": self.operation,
            "ops": self.ops,
            "ops_per_sec": self.ops_per_sec(),
            "p99_micros": self.p99_micros,
            "mean_micros": self.mean_micros,
        })
    }
}

/// Turns a list of per-op microsecond latencies into a [`Row`].
fn summarize(backend: &'static str, operation: &'static str, mut samples: Vec<u64>) -> Row {
    samples.sort_unstable();
    let ops = samples.len();
    let total: u64 = samples.iter().copied().fold(0u64, u64::saturating_add);
    let index = if ops == 0 {
        0
    } else {
        (ops * 99 / 100).min(ops - 1)
    };
    Row {
        backend,
        operation,
        ops,
        total_micros: total,
        p99_micros: samples.get(index).copied().unwrap_or(0),
        mean_micros: if ops == 0 {
            0
        } else {
            total / u64::try_from(ops).unwrap_or(1)
        },
    }
}

fn site() -> SiteId {
    SiteId::new("bench.example").unwrap_or_else(|_| std::process::exit(2))
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bench: cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bench: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> (String, usize, String) {
    let mut url = std::env::var("SPOT_TEST_REDIS_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
    let mut ops = DEFAULT_OPS;
    let mut out = "results.json".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => {
                if let Some(value) = args.next() {
                    url = value;
                }
            }
            "--ops" => {
                if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                    ops = value;
                }
            }
            "--out" => {
                if let Some(value) = args.next() {
                    out = value;
                }
            }
            _ => {}
        }
    }
    (url, ops, out)
}

async fn run() -> Result<(), Box<dyn Error>> {
    let (url, ops, out) = parse_args();
    let site = site();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let mut rows = Vec::new();

    // Built-in cache and coordinator: always available.
    let cache = InProcessCache::with_clock(InProcessConfig::new(1 << 24, 1 << 22), clock.clone());
    rows.extend(cache_rows("in_process", &cache, &site, ops).await?);

    let presence = Arc::new(InMemoryPresence::new());
    let node = NodeId::new("bench-node")?;
    let coordinator: Arc<dyn Coordinator> =
        InProcessCoordinator::start(presence, node, clock.clone()).await?;
    rows.extend(coordinator_rows("in_process", &coordinator, ops).await?);

    // The Redis plugin, when the server answers.
    match RedisConnection::connect(&RedisConfig::new(url.clone())).await {
        Ok(connection) => {
            let redis_cache = RedisCacheStore::with_default(connection.clone());
            rows.extend(cache_rows("redis", &redis_cache, &site, ops).await?);
            let redis: Arc<dyn Coordinator> =
                Arc::new(RedisCoordinator::new(connection, clock.clone()));
            rows.extend(coordinator_rows("redis", &redis, ops).await?);
        }
        Err(error) => {
            eprintln!("bench: Redis not measured ({error}); reporting the built-in backends only");
        }
    }

    let document = serde_json::json!({
        "schema": "spot-cluster-redis-bench/1",
        "url": url,
        "ops": ops,
        "results": rows.iter().map(Row::json).collect::<Vec<_>>(),
    });
    std::fs::write(&out, serde_json::to_vec_pretty(&document)?)?;
    for row in &rows {
        println!(
            "{:<10} {:<16} {:>9} ops/s  p99 {:>6} us  mean {:>5} us",
            row.backend,
            row.operation,
            row.ops_per_sec(),
            row.p99_micros,
            row.mean_micros
        );
    }
    println!("wrote {out}");
    Ok(())
}

/// `set` and `get` against any [`CacheStore`].
async fn cache_rows<C: CacheStore>(
    backend: &'static str,
    cache: &C,
    site: &SiteId,
    ops: usize,
) -> Result<Vec<Row>, Box<dyn Error>> {
    let value = spot_cache::Bytes::from(b"bench-value".to_vec());
    let mut set_samples = Vec::with_capacity(ops);
    for index in 0..ops {
        let key = format!("bench:{index}");
        let start = Instant::now();
        cache.set(site, &key, value.clone(), None).await?;
        set_samples.push(u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    let mut get_samples = Vec::with_capacity(ops);
    for index in 0..ops {
        let key = format!("bench:{index}");
        let start = Instant::now();
        let _ = cache.get(site, &key).await?;
        get_samples.push(u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    Ok(vec![
        summarize(backend, "cache_set", set_samples),
        summarize(backend, "cache_get", get_samples),
    ])
}

/// Lock acquire/release and rate-limit `take` against any [`Coordinator`].
async fn coordinator_rows(
    backend: &'static str,
    coordinator: &Arc<dyn Coordinator>,
    ops: usize,
) -> Result<Vec<Row>, Box<dyn Error>> {
    let mut lock_samples = Vec::with_capacity(ops);
    for _ in 0..ops {
        let start = Instant::now();
        let guard = coordinator
            .lock("bench-lock", "bench-holder", Duration::ZERO)
            .await?;
        guard.unlock().await?;
        lock_samples.push(u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    // A bucket big enough that every take is admitted; the point is the cost of a take.
    let limit = RateLimit {
        capacity: 1_000_000,
        per: Duration::from_secs(1),
    };
    let mut rate_samples = Vec::with_capacity(ops);
    for _ in 0..ops {
        let start = Instant::now();
        match coordinator.take("bench-rate", limit, 1).await? {
            Admit::Allowed { .. } => {}
            Admit::Denied { .. } => {}
        }
        rate_samples.push(u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    Ok(vec![
        summarize(backend, "lock_acquire_release", lock_samples),
        summarize(backend, "rate_take", rate_samples),
    ])
}
