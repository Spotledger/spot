//! Gapless sequences and guard locks under load (the blueprint's "L7 sequence tests under 200
//! sessions").
//!
//! Each test uses up to 200 pooled connections. A server whose `max_connections` is lower (the
//! PostgreSQL default is 100) gets as many as it can spare: the number of concurrent
//! *transactions* stays 200 or more, they queue for connections in the pool, and the test prints
//! the pool size it used. CI should start PostgreSQL with `max_connections >= 250` to run the
//! full 200-connection version.
//!
//! Time on a developer laptop: about 3 s for sequences at read committed, a few seconds for the
//! stricter levels (fewer sessions, with retries), 1 s for guards.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_storage_contract::contract_tests::catalog;
use spot_storage_contract::{
    Catalog, Isolation, LockKey, LockMode, Locks, Name, Sequences, SeriesKey, StorageBackend,
    StorageError, Transaction, Wait,
};
use tokio::sync::Barrier;

const SESSIONS: usize = 200;

/// The tests of this file each take most of the server's connections, so they run one at a time
/// (the harness would otherwise run them in parallel).
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn cat() -> Arc<Catalog> {
    Arc::new(catalog().unwrap())
}

/// A scratch database and a backend with up to `SESSIONS` pooled connections, as many as the
/// server can spare.
async fn backend(wanted: usize) -> Option<(TestDb, PgBackend, usize)> {
    let db = TestDb::create().await?;
    let admin = db.admin_client().await;
    let max: i64 = admin
        .query_one("SELECT current_setting('max_connections')::bigint", &[])
        .await
        .unwrap()
        .get(0);
    let reserved: i64 = admin
        .query_one(
            "SELECT current_setting('superuser_reserved_connections')::bigint",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    // Room for the harness's own connections (admin, raw clients, other test binaries' pools).
    let spare = usize::try_from((max - reserved - 25).max(10)).unwrap();
    let size = wanted.min(spare);
    println!("pool of {size} connections (wanted {wanted}, server max_connections {max})");
    let mut config: PgConfig = db.config();
    config.pool_max_size = size;
    // Hundreds of sessions queue for connections here; the default 5 s would fail them.
    config.pool_wait_timeout = Duration::from_secs(120);
    // ... and opening 200 connections at once can take longer than the default 5 s.
    config.connect_timeout = Duration::from_secs(60);
    let backend = PgBackend::connect(config).await.unwrap();
    Some((db, backend, size))
}

fn options(label: &str, isolation: Isolation) -> spot_storage_contract::TxOptions {
    let mut o = common::options(label, isolation);
    o.lock_timeout = Duration::from_secs(120);
    o.statement_timeout = Duration::from_secs(120);
    o
}

/// A small deterministic generator so every run rolls back the same transactions.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Allocates one number in a transaction at `isolation`, rolls back when `abort` is set, and
/// retries retryable failures (serialization, deadlock) from the start. Returns the committed
/// number, or `None` after a deliberate rollback.
async fn one_allocation(
    backend: &PgBackend,
    series: &SeriesKey,
    isolation: Isolation,
    abort: bool,
) -> Result<Option<u64>, StorageError> {
    let mut attempt = 0u64;
    loop {
        let mut tx = backend.begin(cat(), options("alloc", isolation)).await?;
        let outcome = async {
            let number = tx.allocate(series, 1).await?;
            Ok::<_, StorageError>(number)
        }
        .await;
        match outcome {
            Ok(number) if abort => {
                tx.rollback().await?;
                let _ = number;
                return Ok(None);
            }
            Ok(number) => match tx.commit().await {
                Ok(()) => return Ok(Some(number)),
                Err(e) if e.kind.is_retryable() => {}
                Err(e) => return Err(e),
            },
            Err(e) if e.kind.is_retryable() => {
                tx.rollback().await?;
            }
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(e);
            }
        }
        attempt += 1;
        // Jittered backoff keeps retries from marching in step.
        tokio::time::sleep(Duration::from_millis(1 + (attempt * 7) % 11)).await;
    }
}

/// `sessions` concurrent transactions on each of three series, about 20% of them rolled back.
async fn sequences_under_load(isolation: Isolation, sessions: usize) {
    let Some((_db, backend, _)) = backend(SESSIONS).await else {
        return;
    };
    backend.prepare(&cat()).await.unwrap();
    let series: Vec<SeriesKey> = ["SINV-2026-", "PINV-2026-", "pos:branch7:till2"]
        .iter()
        .map(|s| SeriesKey::new(s).unwrap())
        .collect();
    let committed: Arc<Mutex<BTreeMap<String, Vec<u64>>>> = Arc::default();
    let mut rng = Rng(0x00C0_FFEE_D00D_F00D);
    let mut tasks = Vec::new();
    for round in 0..sessions {
        for s in &series {
            let abort = rng.next().is_multiple_of(5);
            let (backend, s, committed) = (backend.clone(), s.clone(), committed.clone());
            tasks.push(tokio::spawn(async move {
                let got = one_allocation(&backend, &s, isolation, abort)
                    .await
                    .unwrap();
                if let Some(number) = got {
                    committed
                        .lock()
                        .unwrap()
                        .entry(s.as_str().to_owned())
                        .or_default()
                        .push(number);
                }
                round
            }));
        }
    }
    for t in tasks {
        t.await.unwrap();
    }
    let committed = committed.lock().unwrap().clone();
    for s in &series {
        let mut numbers = committed.get(s.as_str()).cloned().unwrap_or_default();
        numbers.sort_unstable();
        let n = numbers.len() as u64;
        assert!(
            n > sessions as u64 / 2,
            "{}: only {n} of {sessions} committed",
            s.as_str()
        );
        assert_eq!(
            numbers,
            (1..=n).collect::<Vec<_>>(),
            "{}: gaps or duplicates",
            s.as_str()
        );
        // And the series is ready to hand out exactly the next one.
        let mut tx = backend
            .begin(cat(), options("check", Isolation::ReadCommitted))
            .await
            .unwrap();
        assert_eq!(tx.allocate(s, 1).await.unwrap(), n + 1, "{}", s.as_str());
        tx.rollback().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_sessions_on_three_series_read_committed() {
    let _one = ONE_AT_A_TIME.lock().await;
    sequences_under_load(Isolation::ReadCommitted, SESSIONS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_repeatable_read_retry_and_stay_gapless() {
    let _one = ONE_AT_A_TIME.lock().await;
    sequences_under_load(Isolation::RepeatableRead, 30).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_serializable_retry_and_stay_gapless() {
    let _one = ONE_AT_A_TIME.lock().await;
    sequences_under_load(Isolation::Serializable, 30).await;
}

/// Blocks of numbers under load: every committed block is contiguous and none overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocks_of_numbers_never_overlap() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some((_db, backend, _)) = backend(SESSIONS).await else {
        return;
    };
    backend.prepare(&cat()).await.unwrap();
    let series = SeriesKey::new("blocks").unwrap();
    let blocks: Arc<Mutex<Vec<(u64, u32)>>> = Arc::default();
    let mut tasks = Vec::new();
    for i in 0..SESSIONS {
        let (backend, series, blocks) = (backend.clone(), series.clone(), blocks.clone());
        tasks.push(tokio::spawn(async move {
            let count = 1 + (i % 5) as u32;
            let mut tx = backend
                .begin(cat(), options("block", Isolation::ReadCommitted))
                .await
                .unwrap();
            let first = tx.allocate(&series, count).await.unwrap();
            if i % 5 == 4 {
                tx.rollback().await.unwrap();
            } else {
                tx.commit().await.unwrap();
                blocks.lock().unwrap().push((first, count));
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let mut blocks = blocks.lock().unwrap().clone();
    blocks.sort_unstable();
    let mut next = 1;
    for (first, count) in blocks {
        assert_eq!(first, next, "a block starts where the previous one ended");
        next = first + u64::from(count);
    }
}

/// The first allocations in a database race to create the bookkeeping table.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_use_races_to_create_the_table_safely() {
    let _one = ONE_AT_A_TIME.lock().await;
    for isolation in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        let Some((_db, backend, _)) = backend(40).await else {
            return;
        };
        backend.prepare(&cat()).await.unwrap();
        let mut tasks = Vec::new();
        for i in 0..30 {
            let backend = backend.clone();
            tasks.push(tokio::spawn(async move {
                let series = SeriesKey::new(&format!("first-{}", i % 3)).unwrap();
                one_allocation(&backend, &series, isolation, false)
                    .await
                    .unwrap()
                    .unwrap()
            }));
        }
        let mut got = Vec::new();
        for t in tasks {
            got.push(t.await.unwrap());
        }
        got.sort_unstable();
        // Three series with ten allocations each: 1..=10 three times.
        let want: Vec<u64> = (1..=10).flat_map(|n| [n, n, n]).collect();
        assert_eq!(got, want, "{isolation:?}");
    }
}

fn guard(key: &str) -> LockKey {
    LockKey {
        namespace: Name::new("counter").unwrap(),
        key: key.to_owned(),
    }
}

/// 200 sessions read, add one and write back a counter under one exclusive guard. Without the
/// guard the read-modify-write loses updates under read committed; with it none is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exclusive_guard_serializes_two_hundred_read_modify_writes() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some((db, backend, _)) = backend(SESSIONS).await else {
        return;
    };
    db.raw_client()
        .await
        .batch_execute("CREATE TABLE counter (id int PRIMARY KEY, v int NOT NULL); INSERT INTO counter VALUES (1, 0);")
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..SESSIONS {
        let backend = backend.clone();
        tasks.push(tokio::spawn(async move {
            let mut tx = backend
                .begin(cat(), options("guarded", Isolation::ReadCommitted))
                .await
                .unwrap();
            tx.lock_key(&guard("1"), LockMode::Exclusive, Wait::Block)
                .await
                .unwrap();
            let rows = tx
                .raw_query("SELECT v FROM counter WHERE id = 1", &[], None)
                .await
                .unwrap();
            let v: i32 = rows[0].get(0);
            // Widen the race window a read-modify-write needs to lose an update.
            tokio::task::yield_now().await;
            tx.raw_execute("UPDATE counter SET v = $1 WHERE id = 1", &[&(v + 1)])
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let v: i32 = db
        .raw_client()
        .await
        .query_one("SELECT v FROM counter WHERE id = 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(v, SESSIONS as i32, "no update was lost");
}

/// Shared holders run together: twenty sessions each take the shared guard and then wait for the
/// others at a barrier. If shared locks excluded one another the barrier would never open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_guards_are_held_concurrently_and_exclude_a_writer() {
    let _one = ONE_AT_A_TIME.lock().await;
    const HOLDERS: usize = 20;
    let Some((_db, backend, _)) = backend(60).await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(HOLDERS + 1));
    let mut tasks = Vec::new();
    for _ in 0..HOLDERS {
        let (backend, barrier) = (backend.clone(), barrier.clone());
        tasks.push(tokio::spawn(async move {
            let mut tx = backend
                .begin(cat(), options("shared", Isolation::ReadCommitted))
                .await
                .unwrap();
            tx.lock_key(&guard("s"), LockMode::Shared, Wait::Block)
                .await
                .unwrap();
            barrier.wait().await; // all twenty hold the guard at this moment
            barrier.wait().await; // hold until the writer was refused
            tx.commit().await.unwrap();
        }));
    }
    tokio::time::timeout(Duration::from_secs(20), barrier.wait())
        .await
        .expect("shared guards overlapped");
    let mut writer = backend
        .begin(cat(), options("writer", Isolation::ReadCommitted))
        .await
        .unwrap();
    let err = writer
        .lock_key(&guard("s"), LockMode::Exclusive, Wait::NoWait)
        .await
        .unwrap_err();
    assert_eq!(err.kind, spot_storage_contract::ErrorKind::LockNotAvailable);
    writer.rollback().await.unwrap();
    barrier.wait().await;
    for t in tasks {
        t.await.unwrap();
    }
    // Everything released: the writer now gets it.
    let mut writer = backend
        .begin(cat(), options("writer", Isolation::ReadCommitted))
        .await
        .unwrap();
    writer
        .lock_key(&guard("s"), LockMode::Exclusive, Wait::NoWait)
        .await
        .unwrap();
    writer.commit().await.unwrap();
}
