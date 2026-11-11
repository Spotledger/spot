//! The L7 "gapless under load" acceptance: 200 concurrent sessions name a document, and the
//! committed names are exactly `1..=n` at ReadCommitted and at Serializable.
//!
//! Runs only when `SPOT_TEST_PG_URL` is set (`TestDb::create` prints `SKIPPED:` otherwise), and
//! creates one scratch database. 30% of the transactions roll back after naming; the numbers
//! they took must return to the series.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use spot_naming::{GapPolicy, Namer, NamingContext, RecordName, StaticPlans};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_storage_contract::{Isolation, StorageBackend, StorageError, Transaction, TxOptions};
use time::Month;

const SESSIONS: usize = 200;

/// The file's tests each take most of the server's connections, so run them one at a time.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn catalog() -> Arc<spot_storage_contract::Catalog> {
    cat()
}

fn opts(label: &str, isolation: Isolation) -> TxOptions {
    let mut o = TxOptions::new(spot_storage_contract::Name::new(label).unwrap(), isolation);
    o.lock_timeout = Duration::from_secs(120);
    o.statement_timeout = Duration::from_secs(120);
    o
}

/// A scratch database and a backend with as many pooled connections as the server can spare.
async fn pg_backend(wanted: usize) -> Option<(TestDb, PgBackend)> {
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
    let spare = usize::try_from((max - reserved - 25).max(10)).unwrap();
    let mut config: PgConfig = db.config();
    config.pool_max_size = wanted.min(spare);
    config.pool_wait_timeout = Duration::from_secs(120);
    config.connect_timeout = Duration::from_secs(60);
    let backend = PgBackend::connect(config).await.unwrap();
    Some((db, backend))
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

fn number_of(name: &RecordName) -> u64 {
    name.as_str().rsplit('-').next().unwrap().parse().unwrap()
}

/// Names one document, rolling back when `abort` is set, retrying retryable failures with backoff.
async fn one_name(
    backend: &PgBackend,
    namer: &Namer,
    doctype: &spot_storage_contract::Name,
    ctx: &NamingContext,
    isolation: Isolation,
    abort: bool,
) -> Result<Option<RecordName>, StorageError> {
    let mut attempt = 0u64;
    loop {
        let mut tx = backend.begin(catalog(), opts("naming", isolation)).await?;
        let outcome = namer
            .name_for(
                doctype,
                &doc(SERIES_DOCTYPE),
                ctx,
                GapPolicy::Tolerant,
                &mut tx,
            )
            .await;
        match outcome {
            Ok(_) if abort => {
                tx.rollback().await?;
                return Ok(None);
            }
            Ok(name) => match tx.commit().await {
                Ok(()) => return Ok(Some(name)),
                Err(e) if e.kind.is_retryable() => {}
                Err(e) => return Err(e),
            },
            Err(e) => {
                let _ = tx.rollback().await;
                match e {
                    spot_naming::NamingError::Storage(e) if e.kind.is_retryable() => {}
                    other => panic!("naming failed: {other}"),
                }
            }
        }
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(1 + (attempt * 7) % 11)).await;
    }
}

/// 200 concurrent sessions name a document on one series; the committed numbers are `1..=n`.
async fn names_under_load(isolation: Isolation) -> u64 {
    let Some((_db, backend)) = pg_backend(SESSIONS).await else {
        return 0;
    };
    let plans = StaticPlans::new().with(SERIES_DOCTYPE, plan("INV-.YYYY-.#####"));
    let namer = Arc::new(Namer::new(Arc::new(plans)));
    let doctype = name(SERIES_DOCTYPE);
    let ctx = ctx(at(2026, Month::June, 1, 12, 0), zone(0, 0));
    let committed: Arc<Mutex<Vec<u64>>> = Arc::default();

    let mut rng = Rng(0x00C0_FFEE_D00D_F00D);
    let mut tasks = Vec::new();
    for _ in 0..SESSIONS {
        let abort = rng.next() % 10 < 3;
        let backend = backend.clone();
        let namer = Arc::clone(&namer);
        let doctype = doctype.clone();
        let ctx = ctx.clone();
        let committed = Arc::clone(&committed);
        tasks.push(tokio::spawn(async move {
            let got = one_name(&backend, &namer, &doctype, &ctx, isolation, abort)
                .await
                .unwrap();
            if let Some(name) = got {
                committed.lock().unwrap().push(number_of(&name));
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }

    let mut numbers = committed.lock().unwrap().clone();
    numbers.sort_unstable();
    let n = numbers.len() as u64;
    assert!(n > SESSIONS as u64 / 2, "only {n} of {SESSIONS} committed");
    assert_eq!(
        numbers,
        (1..=n).collect::<Vec<_>>(),
        "committed names have a gap or a duplicate"
    );
    n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_sessions_stay_gapless_read_committed() {
    let _one = ONE_AT_A_TIME.lock().await;
    let n = names_under_load(Isolation::ReadCommitted).await;
    if n > 0 {
        println!("read committed: {n} committed names, exactly 1..={n}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_sessions_stay_gapless_serializable() {
    let _one = ONE_AT_A_TIME.lock().await;
    let n = names_under_load(Isolation::Serializable).await;
    if n > 0 {
        println!("serializable: {n} committed names, exactly 1..={n}");
    }
}
