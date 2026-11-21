//! The PostgreSQL store against a real server (`SPOT_TEST_PG_URL`; skipped with a message when
//! unset, a failure when `SPOT_REQUIRE_PG=1`). The case table runs against the in-memory
//! reference store first and then against PostgreSQL, so the two are held to one contract.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::thread;

use common::*;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_rehearsal::Fingerprint;
use spot_tenant_move::{
    AllowedRegions, BoxFuture, Chunk, Error, FingerprintSource, MemoryMoveStore, MoveEngine,
    MoveId, MovePlan, MoveState, MoveStore, PlacementPort, QuiescePort, Result, Side, TransferPort,
};
use spot_tenant_move_pg::PgMoveStore;

// ---------------------------------------------------------------------------
// The shared case table
// ---------------------------------------------------------------------------

type Case = (&'static str, fn(&dyn MoveStore));

fn cases() -> Vec<Case> {
    vec![
        ("get on an empty store is None", |s| {
            assert!(s.get(&tenant(1)).unwrap().is_none());
        }),
        ("create then get returns the record at revision 0", |s| {
            let mut r = record(1);
            r.revision = 9; // ignored: a new record always starts at 0
            s.create(r).unwrap();
            let got = s.get(&tenant(1)).unwrap().unwrap();
            assert_eq!(got, record(1));
            assert_eq!(got.revision, 0);
        }),
        ("create twice for one tenant is AlreadyExists", |s| {
            s.create(record(1)).unwrap();
            let err = s.create(record(1)).unwrap_err();
            assert!(matches!(err, Error::AlreadyExists { .. }), "{err:?}");
            assert_eq!(s.get(&tenant(1)).unwrap().unwrap(), record(1));
        }),
        (
            "save bumps the revision and returns the stored record",
            |s| {
                s.create(record(1)).unwrap();
                let saved = s.save(advanced(record(1), 2_000), 0).unwrap();
                assert_eq!(saved.revision, 1);
                assert_eq!(saved.state, MoveState::Quiesced);
                assert_eq!(s.get(&tenant(1)).unwrap().unwrap(), saved);
            },
        ),
        (
            "save with a stale revision is a store failure and changes nothing",
            |s| {
                s.create(record(1)).unwrap();
                s.save(advanced(record(1), 2_000), 0).unwrap();
                let err = s.save(advanced(record(1), 3_000), 0).unwrap_err();
                assert!(matches!(err, Error::StoreFailed { .. }), "{err:?}");
                let got = s.get(&tenant(1)).unwrap().unwrap();
                assert_eq!(got.revision, 1);
                assert_eq!(got.updated_at, 2_000);
            },
        ),
        ("save for an unknown tenant is a store failure", |s| {
            let err = s.save(record(2), 0).unwrap_err();
            assert!(matches!(err, Error::StoreFailed { .. }), "{err:?}");
            assert!(s.get(&tenant(2)).unwrap().is_none());
        }),
        ("tenants do not see each other", |s| {
            s.create(record(1)).unwrap();
            s.create(record(2)).unwrap();
            s.save(advanced(record(1), 2_000), 0).unwrap();
            assert_eq!(s.get(&tenant(2)).unwrap().unwrap(), record(2));
        }),
        ("a sequence of saves keeps the whole history", |s| {
            s.create(record(1)).unwrap();
            let mut current = record(1);
            for (i, at) in [2_000_u64, 3_000, 4_000, 5_000].into_iter().enumerate() {
                let saved = s
                    .save(advanced(current.clone(), at), u64::try_from(i).unwrap())
                    .unwrap();
                current = saved;
            }
            let got = s.get(&tenant(1)).unwrap().unwrap();
            assert_eq!(got.state, MoveState::Verified);
            assert_eq!(got.revision, 4);
            assert_eq!(got.history.len(), 5);
        }),
    ]
}

#[test]
fn the_memory_store_passes_the_case_table() {
    for (name, case) in cases() {
        let store = MemoryMoveStore::new();
        println!("memory: {name}");
        case(&store);
    }
}

#[test]
fn the_postgres_store_passes_the_same_case_table() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open(fx.db.config());
    for (name, case) in cases() {
        println!("postgres: {name}");
        // Each case starts from an empty table; the history is cleared by recreating the rows'
        // owner, so the scratch database is wiped between cases with the admin connection.
        wipe(&fx);
        case(&store);
    }
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

/// Empties both tables. The history table is append-only (a trigger refuses DELETE), so the
/// trigger is dropped and recreated by `prepare`.
fn wipe(fx: &Fixture) {
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .batch_execute(
                "DROP TRIGGER IF EXISTS spot_tenant_move_event_append_only \
                     ON spot_control.spot_tenant_move_event; \
                 TRUNCATE spot_control.spot_tenant_move_event, spot_control.spot_tenant_move; \
                 CREATE TRIGGER spot_tenant_move_event_append_only \
                     BEFORE UPDATE OR DELETE ON spot_control.spot_tenant_move_event \
                     FOR EACH ROW EXECUTE FUNCTION spot_control.spot_tenant_move_event_immutable();",
            )
            .await
            .unwrap();
    });
}

// ---------------------------------------------------------------------------
// PostgreSQL-specific behaviour
// ---------------------------------------------------------------------------

#[test]
fn two_operators_saving_the_same_revision_exactly_one_wins() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open(fx.db.config());
    store.create(record(1)).unwrap();
    let store = Arc::new(store);
    let handles: Vec<_> = (0..2_u64)
        .map(|i| {
            let store = Arc::clone(&store);
            thread::spawn(move || store.save(advanced(record(1), 2_000 + i), 0))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert_eq!(store.get(&tenant(1)).unwrap().unwrap().revision, 1);
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

#[test]
fn history_rows_equal_the_records_own_history_after_saves() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open(fx.db.config());
    store.create(record(1)).unwrap();
    let mut current = record(1);
    for (i, at) in [2_000_u64, 3_000, 4_000].into_iter().enumerate() {
        current = store
            .save(advanced(current, at), u64::try_from(i).unwrap())
            .unwrap();
    }
    let rows: Vec<(i64, String, i64)> = fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .query(
                "SELECT seq, state, at FROM spot_control.spot_tenant_move_event \
                 WHERE tenant = $1 ORDER BY seq",
                &[&tenant(1).to_string()],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect()
    });
    let expected: Vec<(i64, String, i64)> = current
        .history
        .iter()
        .enumerate()
        .map(|(i, e)| {
            (
                i64::try_from(i).unwrap(),
                e.state.name().to_owned(),
                i64::try_from(e.at).unwrap(),
            )
        })
        .collect();
    assert_eq!(rows, expected);
    // The history is append-only.
    let refused = fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .execute("DELETE FROM spot_control.spot_tenant_move_event", &[])
            .await
    });
    assert!(refused.is_err());
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

#[test]
fn a_save_whose_history_conflicts_rolls_the_state_change_back() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open(fx.db.config());
    store.create(record(1)).unwrap();
    let one = store.save(advanced(record(1), 2_000), 0).unwrap();
    // A record with a shorter history than the stored one is refused, and the row is unchanged.
    let mut shorter = advanced(record(1), 3_000);
    shorter.history.truncate(1);
    let err = store.save(shorter, 1).unwrap_err();
    assert!(matches!(err, Error::StoreFailed { .. }), "{err:?}");
    assert_eq!(store.get(&tenant(1)).unwrap().unwrap(), one);
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

#[test]
fn state_survives_a_restart() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open(fx.db.config());
    store.create(record(1)).unwrap();
    let saved = store.save(advanced(record(1), 2_000), 0).unwrap();
    drop(store);
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
    drop(bridge);
    let (bridge, store) = open(fx.db.config());
    assert_eq!(store.get(&tenant(1)).unwrap().unwrap(), saved);
    // Preparing again keeps the rows.
    store.prepare().unwrap();
    assert_eq!(store.get(&tenant(1)).unwrap().unwrap(), saved);
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

#[test]
fn a_call_from_inside_a_runtime_is_refused_not_deadlocked() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open(fx.db.config());
    let err = fx.rt.block_on(async { store.get(&tenant(1)) }).unwrap_err();
    let Error::StoreFailed { reason, .. } = &err else {
        panic!("{err:?}")
    };
    assert!(reason.contains("pg_bridge.wrong_thread"), "{reason}");
    let err = fx
        .rt
        .block_on(async { store.create(record(1)) })
        .unwrap_err();
    assert!(matches!(err, Error::StoreFailed { .. }));
    assert!(store.get(&tenant(1)).unwrap().is_none());
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

#[test]
fn building_the_store_inside_a_current_thread_runtime_is_refused() {
    let Some(fx) = fixture() else { return };
    let pool = pool_of(fx.db.config(), 6);
    let bridge = Arc::new(Bridge::new(BridgeConfig::default(), pool.clone()).unwrap());
    let current = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let err = current
        .block_on(async { PgMoveStore::new(Arc::clone(&bridge), &pool) })
        .unwrap_err();
    assert_eq!(err.code(), "tenant_move_pg.bad_setup");
    // A bridge too large for a smaller pool is refused as well.
    let small = pool_of(fx.db.config(), 3);
    let err = PgMoveStore::new(bridge, &small).unwrap_err();
    assert_eq!(err.code(), "tenant_move_pg.bad_setup");
}

#[test]
fn an_unreachable_database_is_a_store_failure_and_leaves_no_row() {
    let Some(fx) = fixture() else { return };
    let (bridge, good) = open(fx.db.config());
    let mut dead = fx.db.config();
    dead.host = "127.0.0.1".to_owned();
    dead.port = 1;
    let pool = pool_of(dead, 6);
    let down = Arc::new(Bridge::new(BridgeConfig::default(), pool.clone()).unwrap());
    let store = PgMoveStore::new(Arc::clone(&down), &pool).unwrap();
    let err = store.create(record(1)).unwrap_err();
    assert!(matches!(err, Error::StoreFailed { .. }), "{err:?}");
    let err = store.get(&tenant(1)).unwrap_err();
    assert!(matches!(err, Error::StoreFailed { .. }), "{err:?}");
    assert!(good.get(&tenant(1)).unwrap().is_none());
    down.shutdown(std::time::Duration::from_secs(2)).unwrap();
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

#[test]
fn no_secret_or_connection_string_appears_in_any_error_text() {
    let Some(fx) = fixture() else { return };
    let mut dead = fx.db.config();
    let secret = dead.password.clone();
    dead.host = "127.0.0.1".to_owned();
    dead.port = 1;
    let pool = pool_of(dead.clone(), 6);
    let down = Arc::new(Bridge::new(BridgeConfig::default(), pool.clone()).unwrap());
    let store = PgMoveStore::new(Arc::clone(&down), &pool).unwrap();
    let errors = [
        store.create(record(1)).unwrap_err(),
        store.get(&tenant(1)).unwrap_err(),
        store.save(record(1), 0).unwrap_err(),
    ];
    for error in &errors {
        for text in [format!("{error}"), format!("{error:?}")] {
            for forbidden in [
                secret.as_str(),
                dead.host.as_str(),
                dead.user.as_str(),
                dead.dbname.as_str(),
                "postgres://",
                "SELECT",
                "INSERT",
                "UPDATE",
            ] {
                if forbidden.is_empty() {
                    continue;
                }
                assert!(!text.contains(forbidden), "{forbidden:?} leaked in {text}");
            }
        }
    }
    let setup = store.prepare().unwrap_err();
    let text = format!("{setup} {setup:?}");
    assert!(!text.contains(&secret) && !text.contains("postgres://"));
    down.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

#[test]
fn a_stored_document_that_no_longer_deserialises_is_a_store_failure() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open(fx.db.config());
    store.create(record(1)).unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .execute(
                "UPDATE spot_control.spot_tenant_move SET record = '{\"id\": 5}'::jsonb",
                &[],
            )
            .await
            .unwrap();
    });
    let err = store.get(&tenant(1)).unwrap_err();
    assert!(matches!(err, Error::StoreFailed { .. }), "{err:?}");
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

// ---------------------------------------------------------------------------
// The engine refuses an abort after cut-over and leaves the stored record unchanged
// ---------------------------------------------------------------------------

struct Refusing;

impl TransferPort for Refusing {
    fn export_chunk<'a>(
        &'a self,
        _: &'a MovePlan,
        _: Option<u64>,
    ) -> BoxFuture<'a, Result<Option<Chunk>>> {
        Box::pin(async { panic!("unused") })
    }
    fn import_chunk<'a>(&'a self, _: &'a MovePlan, _: &'a Chunk) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { panic!("unused") })
    }
    fn finish_import<'a>(&'a self, _: &'a MovePlan) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { panic!("unused") })
    }
    fn cleanup<'a>(&'a self, _: &'a MovePlan) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { panic!("unused") })
    }
}

impl QuiescePort for Refusing {
    fn quiesce<'a>(&'a self, _: &'a MovePlan, _: &'a MoveId, _: u64) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { panic!("unused") })
    }
    fn resume<'a>(&'a self, _: &'a MovePlan, _: &'a MoveId, _: u64) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { panic!("an abort after cut-over must not resume the source") })
    }
}

impl PlacementPort for Refusing {
    fn cut_over<'a>(&'a self, _: &'a MovePlan, _: u64) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { panic!("unused") })
    }
    fn restore<'a>(&'a self, _: &'a MovePlan) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { panic!("an abort after cut-over must not restore the placement") })
    }
}

impl FingerprintSource for Refusing {
    fn measure<'a>(&'a self, _: &'a MovePlan, _: Side) -> BoxFuture<'a, Result<Fingerprint>> {
        Box::pin(async { panic!("unused") })
    }
}

/// Polls a future that must finish on its first poll (the engine's refusal does not wait).
fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("the future was expected to finish at once"),
    }
}

#[test]
fn an_abort_after_cut_over_is_refused_and_the_stored_record_is_unchanged() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open(fx.db.config());
    store.create(record(1)).unwrap();
    let mut current = record(1);
    for (i, at) in [2_000_u64, 3_000, 4_000, 5_000, 6_000]
        .into_iter()
        .enumerate()
    {
        current = store
            .save(advanced(current, at), u64::try_from(i).unwrap())
            .unwrap();
    }
    assert_eq!(current.state, MoveState::CutOver);
    let before = store.get(&tenant(1)).unwrap().unwrap();
    let refusing = Refusing;
    let engine = MoveEngine {
        store: &store,
        transfer: &refusing,
        quiesce: &refusing,
        placement: &refusing,
        residency: &AllowedRegions::new(),
        fingerprints: &refusing,
    };
    let err = ready(engine.abort(&tenant(1), 9_000, "operator")).unwrap_err();
    assert!(matches!(err, Error::AbortTooLate { .. }), "{err:?}");
    assert_eq!(store.get(&tenant(1)).unwrap().unwrap(), before);
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();
}
