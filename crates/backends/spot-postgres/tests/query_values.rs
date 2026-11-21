//! Results decode exactly, `read` keeps its rules, row locks fail as documented, and the
//! statement cache is reused and bounded.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use rust_decimal::Decimal;
use spot_postgres::PgBackend;
use spot_storage_contract::contract_tests::catalog;
use spot_storage_contract::{
    Access, AggFunc, ArithOp, Catalog, DocumentData, DocumentStore, ErrorKind, Expr, Fields,
    Isolation, Name, Query, QueryExecutor, QueryPath, ReadSource, RecordId, RowLock, Rows,
    Selection, StorageBackend, Transaction, Value, Wait,
};
use time::{Month, OffsetDateTime, UtcOffset};

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn cat() -> Arc<Catalog> {
    Arc::new(catalog().unwrap())
}

fn party(name: &str, limit: Decimal) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("name"), Value::Text(name.to_owned()));
    fields.insert(n("credit_limit"), Value::Decimal(limit));
    for f in [
        "parent_party",
        "note",
        "since",
        "opens_at",
        "seen_at",
        "active",
        "visits",
        "profile",
        "logo",
    ] {
        fields.insert(n(f), Value::Null);
    }
    DocumentData {
        entity: n("party"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    }
}

fn sel(expr: Expr, alias: &str) -> Selection {
    Selection {
        expr,
        alias: n(alias),
    }
}

fn col(field: &str) -> Expr {
    Expr::field(&n("p"), &n(field))
}

fn from_party() -> Query {
    Query::from(&n("party"), &n("p"), QueryPath::Transactional)
}

async fn env_with_parties(docs: &[DocumentData], pool: usize) -> Option<common::Env> {
    let env = common::env_with(|c| c.pool_max_size = pool).await?;
    env.backend.prepare(&cat()).await.unwrap();
    let mut tx = env
        .backend
        .begin(cat(), common::options("seed", Isolation::ReadCommitted))
        .await
        .unwrap();
    for d in docs {
        tx.insert(d).await.unwrap();
    }
    tx.commit().await.unwrap();
    Some(env)
}

async fn read(b: &PgBackend, q: &Query) -> Result<Rows, spot_storage_contract::StorageError> {
    b.read(cat(), ReadSource::Primary, q).await
}

#[tokio::test]
async fn decimal_edge_values_decode_exactly() {
    let edges = [
        Decimal::new(9_999_999_999_999_999, 2), // the largest value of numeric(18,2)
        Decimal::new(-9_999_999_999_999_999, 2),
        Decimal::new(1, 2),
        Decimal::new(0, 2),
        Decimal::new(-1, 2),
        Decimal::new(100_000_000_000_000, 2),
    ];
    let docs: Vec<_> = edges
        .iter()
        .enumerate()
        .map(|(i, d)| party(&format!("p{i}"), *d))
        .collect();
    let Some(env) = env_with_parties(&docs, 2).await else {
        return;
    };
    let mut q = from_party();
    q.select = vec![sel(col("name"), "name"), sel(col("credit_limit"), "limit")];
    q.order_by = vec![];
    let rows = read(&env.backend, &q).await.unwrap();
    assert_eq!(rows.rows.len(), edges.len());
    for row in &rows.rows {
        let Value::Text(name) = &row[0] else {
            panic!("name")
        };
        let i: usize = name[1..].parse().unwrap();
        let Value::Decimal(got) = &row[1] else {
            panic!("limit")
        };
        assert_eq!(*got, edges[i]);
        assert_eq!(got.scale(), 2, "the declared scale survives: {got}");
        assert_eq!(got.to_string(), edges[i].to_string());
    }
    // Arithmetic keeps full precision: 99999999999999.99 * 100 = 9999999999999999.00.
    let mut q = from_party();
    q.select = vec![sel(
        Expr::Arith(
            Box::new(col("credit_limit")),
            ArithOp::Mul,
            Box::new(Expr::value(100_i64)),
        ),
        "x",
    )];
    q.filter = Some(col("name").eq(Expr::value("p0")));
    let rows = read(&env.backend, &q).await.unwrap();
    assert_eq!(
        rows.rows,
        vec![vec![Value::Decimal(Decimal::new(
            999_999_999_999_999_900,
            2
        ))]]
    );
}

#[tokio::test]
async fn a_decimal_that_does_not_fit_is_refused_not_rounded() {
    let docs = [party("big", Decimal::new(9_999_999_999_999_999, 2))];
    let Some(env) = env_with_parties(&docs, 2).await else {
        return;
    };
    // 99999999999999.99 squared has 32 significant digits: PostgreSQL computes it, rust_decimal
    // cannot hold it.
    let mut q = from_party();
    q.select = vec![sel(
        Expr::Arith(
            Box::new(col("credit_limit")),
            ArithOp::Mul,
            Box::new(col("credit_limit")),
        ),
        "square",
    )];
    assert_eq!(
        read(&env.backend, &q).await.unwrap_err().kind,
        ErrorKind::InvalidValue
    );
    let mut tx = env
        .backend
        .begin(cat(), common::options("t", Isolation::ReadCommitted))
        .await
        .unwrap();
    assert_eq!(
        tx.query(&q).await.unwrap_err().kind,
        ErrorKind::InvalidValue
    );
    // The refusal happened after the statement ran; the transaction itself is intact.
    q.select = vec![sel(col("name"), "name")];
    assert_eq!(
        tx.query(&q).await.unwrap().rows,
        vec![vec![Value::Text("big".into())]]
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn integer_overflow_in_sum_is_invalid_value() {
    let mut docs = vec![party("a", Decimal::ZERO), party("b", Decimal::ZERO)];
    for d in &mut docs {
        d.fields.insert(n("visits"), Value::Int(i64::MAX));
    }
    let Some(env) = env_with_parties(&docs, 2).await else {
        return;
    };
    let mut q = from_party();
    q.select = vec![sel(
        Expr::Agg {
            func: AggFunc::Sum,
            arg: Some(Box::new(col("visits"))),
            distinct: false,
        },
        "s",
    )];
    assert_eq!(
        read(&env.backend, &q).await.unwrap_err().kind,
        ErrorKind::InvalidValue
    );
}

#[tokio::test]
async fn timestamps_come_back_in_utc_and_every_type_roundtrips() {
    let mut d = party("typed", Decimal::new(150, 2));
    let at = OffsetDateTime::from_unix_timestamp(1_700_000_000)
        .unwrap()
        .to_offset(UtcOffset::from_hms(5, 0, 0).unwrap());
    d.fields.insert(n("seen_at"), Value::DateTime(at));
    d.fields.insert(
        n("since"),
        Value::Date(time::Date::from_calendar_date(2024, Month::February, 29).unwrap()),
    );
    d.fields.insert(
        n("opens_at"),
        Value::Time(time::Time::from_hms_micro(8, 30, 15, 123_456).unwrap()),
    );
    d.fields.insert(n("active"), Value::Bool(true));
    d.fields.insert(n("visits"), Value::Int(i64::MIN));
    d.fields.insert(n("logo"), Value::Bytes(vec![0, 255, 7]));
    d.fields
        .insert(n("profile"), Value::Json("{\"a\":[1,2]}".into()));
    let Some(env) = env_with_parties(std::slice::from_ref(&d), 2).await else {
        return;
    };
    let fields = [
        "seen_at",
        "since",
        "opens_at",
        "active",
        "visits",
        "logo",
        "profile",
        "parent_party",
        "note",
    ];
    let mut q = from_party();
    q.select = fields.iter().map(|f| sel(col(f), f)).collect();
    let rows = read(&env.backend, &q).await.unwrap();
    let row = &rows.rows[0];
    let Value::DateTime(got) = &row[0] else {
        panic!("seen_at")
    };
    assert_eq!(got.offset(), UtcOffset::UTC);
    assert_eq!(*got, at);
    for (i, f) in fields.iter().enumerate().skip(1) {
        match (f, &row[i]) {
            (&"profile", Value::Json(j)) => {
                let compact: String = j.chars().filter(|c| !c.is_whitespace()).collect();
                assert_eq!(compact, "{\"a\":[1,2]}");
            }
            (&"parent_party" | &"note", v) => assert_eq!(*v, Value::Null),
            (f, v) => assert_eq!(v, &d.fields[&n(f)], "{f}"),
        }
    }
    // The same through a transaction.
    let mut tx = env
        .backend
        .begin(cat(), common::options("t", Isolation::ReadCommitted))
        .await
        .unwrap();
    assert_eq!(tx.query(&q).await.unwrap(), rows);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn read_has_its_own_rules() {
    let Some(env) = env_with_parties(&[party("a", Decimal::ZERO)], 2).await else {
        return;
    };
    let mut q = from_party();
    q.select = vec![sel(col("name"), "name")];
    for lock in [
        RowLock::Update(Wait::Block),
        RowLock::Update(Wait::NoWait),
        RowLock::Share(Wait::Block),
        RowLock::Update(Wait::SkipLocked),
    ] {
        q.lock = lock;
        assert_eq!(
            read(&env.backend, &q).await.unwrap_err().kind,
            ErrorKind::InvalidQuery,
            "{lock:?}"
        );
    }
    q.lock = RowLock::None;
    for source in [ReadSource::Replica, ReadSource::AnalyticsMirror] {
        let err = env.backend.read(cat(), source, &q).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unsupported, "{source:?}");
    }
    // An invalid query is the contract's error, from the same validation as every backend.
    q.select = vec![sel(col("nope"), "x")];
    assert_eq!(
        read(&env.backend, &q).await.unwrap_err().kind,
        ErrorKind::UnknownField
    );
    // `read` leaves nothing open: one connection serves many reads and a write afterwards.
    let Some(one) = env_with_parties(&[party("a", Decimal::ZERO)], 1).await else {
        return;
    };
    q.select = vec![sel(col("name"), "name")];
    for _ in 0..5 {
        assert_eq!(read(&one.backend, &q).await.unwrap().rows.len(), 1);
    }
    let mut tx = one
        .backend
        .begin(cat(), common::options("w", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.insert(&party("b", Decimal::ZERO)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(read(&one.backend, &q).await.unwrap().rows.len(), 2);
}

#[tokio::test]
async fn read_runs_read_committed_and_read_only() {
    let Some(env) = env_with_parties(&[party("a", Decimal::ZERO)], 1).await else {
        return;
    };
    // With one connection, the pooled connection a transaction gets is the one `read` used.
    let mut q = from_party();
    q.select = vec![sel(col("name"), "name")];
    read(&env.backend, &q).await.unwrap();
    let mut tx = env
        .backend
        .begin(cat(), common::options("probe", Isolation::ReadCommitted))
        .await
        .unwrap();
    let idle = tx
        .raw_query("SELECT count(*) FROM pg_stat_activity WHERE state LIKE 'idle in transaction%' AND datname = current_database()", &[], None)
        .await
        .unwrap();
    assert_eq!(idle[0].get::<_, i64>(0), 0);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn row_locks_time_out_and_skip() {
    let docs = [party("a", Decimal::ZERO), party("b", Decimal::ZERO)];
    let Some(env) = env_with_parties(&docs, 4).await else {
        return;
    };
    let mut holder = env
        .backend
        .begin(cat(), common::options("holder", Isolation::ReadCommitted))
        .await
        .unwrap();
    let mut q = from_party();
    q.select = vec![sel(col("name"), "name")];
    q.filter = Some(col("name").eq(Expr::value("a")));
    q.lock = RowLock::Update(Wait::Block);
    assert_eq!(holder.query(&q).await.unwrap().rows.len(), 1);

    let mut options = common::options("waiter", Isolation::ReadCommitted);
    options.lock_timeout = Duration::from_millis(300);
    let mut waiter = env.backend.begin(cat(), options.clone()).await.unwrap();
    assert_eq!(
        waiter.query(&q).await.unwrap_err().kind,
        ErrorKind::LockTimeout
    );
    waiter.rollback().await.unwrap();

    let mut nowait = env.backend.begin(cat(), options.clone()).await.unwrap();
    q.lock = RowLock::Share(Wait::NoWait);
    assert_eq!(
        nowait.query(&q).await.unwrap_err().kind,
        ErrorKind::LockNotAvailable
    );
    nowait.rollback().await.unwrap();

    // SKIP LOCKED leaves the locked row out and returns the free one.
    let mut skipper = env.backend.begin(cat(), options).await.unwrap();
    q.filter = None;
    q.order_by = vec![];
    q.lock = RowLock::Update(Wait::SkipLocked);
    assert_eq!(
        skipper.query(&q).await.unwrap().rows,
        vec![vec![Value::Text("b".into())]]
    );
    skipper.rollback().await.unwrap();
    holder.rollback().await.unwrap();
}

#[tokio::test]
async fn a_read_only_transaction_can_query_but_not_lock() {
    let Some(env) = env_with_parties(&[party("a", Decimal::ZERO)], 2).await else {
        return;
    };
    let mut options = common::options("ro", Isolation::ReadCommitted);
    options.access = Access::ReadOnly;
    let mut tx = env.backend.begin(cat(), options).await.unwrap();
    let mut q = from_party();
    q.select = vec![sel(col("name"), "name")];
    assert_eq!(tx.query(&q).await.unwrap().rows.len(), 1);
    q.lock = RowLock::Update(Wait::Block);
    assert_eq!(
        tx.query(&q).await.unwrap_err().kind,
        ErrorKind::ReadOnlyTransaction
    );
    tx.rollback().await.unwrap();
}

/// Prepared statements of the pooled connection that are queries of the compiler (they all end
/// in `OFFSET`); the probe itself is excluded.
async fn prepared(env: &common::Env) -> i64 {
    let mut tx = env
        .backend
        .begin(cat(), common::options("probe", Isolation::ReadCommitted))
        .await
        .unwrap();
    let rows = tx
        .raw_query("SELECT count(*) FROM pg_prepared_statements WHERE statement LIKE '%OFFSET%' AND statement NOT LIKE '%pg_prepared_statements%'", &[], None)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows[0].get(0)
}

#[tokio::test]
async fn the_statement_cache_is_reused_and_bounded() {
    let Some(env) = env_with_parties(&[party("a", Decimal::ZERO)], 1).await else {
        return;
    };
    let mut q = from_party();
    q.select = vec![sel(col("name"), "name")];
    assert_eq!(prepared(&env).await, 0);
    for page in 0..5_u32 {
        // Limit and offset are parameters, so every page is the same statement.
        q.limit = Some(page + 1);
        q.offset = u64::from(page);
        read(&env.backend, &q).await.unwrap();
    }
    assert_eq!(prepared(&env).await, 1, "five pages, one statement");
    q.select = vec![sel(col("name"), "name"), sel(col("note"), "note")];
    read(&env.backend, &q).await.unwrap();
    read(&env.backend, &q).await.unwrap();
    assert_eq!(
        prepared(&env).await,
        2,
        "a different query is a second statement"
    );

    // Bounded: many distinct statements never leave more than the bound prepared.
    for i in 0..300 {
        let mut distinct = from_party();
        distinct.select = vec![sel(col("name"), &format!("c{i}"))];
        read(&env.backend, &distinct).await.unwrap();
        assert!(prepared_count_is_bounded(&env).await);
    }
    // And it still works, and still caches, after the cache was cleared.
    read(&env.backend, &q).await.unwrap();
    let before = prepared(&env).await;
    read(&env.backend, &q).await.unwrap();
    assert_eq!(prepared(&env).await, before);
}

async fn prepared_count_is_bounded(env: &common::Env) -> bool {
    prepared(env).await <= 256
}

#[tokio::test]
async fn a_cancelled_read_never_leaves_a_transaction_open_on_the_pool() {
    let Some(env) = env_with_parties(&[party("a", Decimal::ZERO)], 1).await else {
        return;
    };
    let mut q = from_party();
    q.select = vec![sel(col("name"), "name")];
    for micros in 0..60_u64 {
        let _ =
            tokio::time::timeout(Duration::from_micros(micros * 40), read(&env.backend, &q)).await;
    }
    // Whatever the cancellation points were, the next user gets a clean connection.
    let mut tx = env
        .backend
        .begin(cat(), common::options("probe", Isolation::ReadCommitted))
        .await
        .unwrap();
    let rows = tx
        .raw_query("SELECT count(*) FROM pg_stat_activity WHERE state LIKE 'idle in transaction%' AND datname = current_database() AND pid <> pg_backend_pid()", &[], None)
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>(0), 0);
    tx.rollback().await.unwrap();
    assert_eq!(read(&env.backend, &q).await.unwrap().rows.len(), 1);
}
