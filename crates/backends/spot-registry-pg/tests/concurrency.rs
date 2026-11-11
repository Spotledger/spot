//! Several registries over one PostgreSQL database (`SPOT_TEST_PG_URL`; skipped with a message when
//! unset, a failure when `SPOT_REQUIRE_PG=1`): races for one name, one release, one removal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    unused_imports
)]

mod common;

use std::sync::{Arc, Barrier};
use std::time::Duration;

use common::*;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_registry::*;
use spot_registry_pg::PgIndexStore;

fn memory() -> Arc<MemoryIndexStore> {
    Arc::new(MemoryIndexStore::default())
}

/// Real signed releases from a throwaway in-memory registry.
fn source() -> Setup<MemoryIndexStore> {
    Setup::new(memory())
}

const RICH: &str = "requires_host = \"^0.1\"\ndepends = [{ app = \"base\", version = \"^2\" }]\n\
    [capabilities]\nemail = true\nfiles = \"read\"\nnetwork = [\"example.test:443\"]\n\
    doctypes = { read = [\"Document\"], write = [\"Document\"] }\n";

fn rich_setup() -> Setup<MemoryIndexStore> {
    Setup::with_policy(
        memory(),
        RegistryPolicy {
            capabilities: spot_bundle::Policy {
                allow_email: true,
                ..Default::default()
            },
            ..Default::default()
        },
    )
}

// ---------------------------------------------------------------------------
// Several registries, one database
// ---------------------------------------------------------------------------

#[test]
fn two_registry_instances_racing_for_one_name_leave_one_owner() {
    let Some(fx) = fixture() else { return };
    let (b1, store1) = open_db(fx.db.config());
    let (b2, store2) = open_db(fx.db.config());
    let s1 = Setup::new(store1.clone());
    // The second instance shares the blobs, trust and audit of the first but indexes through its
    // own store (its own bridge and pool) on the same database.
    let second = Arc::new(Registry::new(
        store2.clone(),
        s1.blobs.clone(),
        RegistryPolicy::default(),
        s1.trust.clone(),
        s1.audit.clone(),
    ));
    let first = s1.registry.clone();
    let barrier = Arc::new(Barrier::new(9));
    let threads: Vec<_> = (12..20_u8)
        .map(|id| {
            let registry = if id % 2 == 0 {
                first.clone()
            } else {
                second.clone()
            };
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let b = bytes("demo", "1.0.0", "");
                let p = proof(&b, id);
                barrier.wait();
                run(registry.publish(b, p.signature, p.chain, NOW))
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.code == "registry.not_owner" || e.code == "registry.version_conflict")
    );
    assert_eq!(store1.digests().unwrap().len(), 1);
    assert_eq!(store2.digests().unwrap().len(), 1);
    assert_eq!(s1.audit.records().unwrap().len(), 1);
    let owner = store1.ownership(&app("demo")).unwrap().unwrap();
    assert_eq!(Some(owner), store2.ownership(&app("demo")).unwrap());
    b1.shutdown(Duration::from_secs(2)).unwrap();
    b2.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn concurrent_versions_never_allow_a_downgrade_or_a_conflicting_identity() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let s = Setup::new(store.clone());
    s.publish("demo", "1.0.0", "", 12);
    let barrier = Arc::new(Barrier::new(3));
    let threads: Vec<_> = ["2.0.0", "3.0.0"]
        .into_iter()
        .map(|v| {
            let registry = s.registry.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let b = bytes("demo", v, "");
                let p = proof(&b, 12);
                barrier.wait();
                run(registry.publish(b, p.signature, p.chain, NOW))
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(results[1].is_ok());
    assert!(
        results[0].is_ok()
            || results[0].as_ref().unwrap_err().code == "registry.version_not_increasing"
    );
    assert_eq!(
        s.registry
            .latest(&app("demo"), false)
            .unwrap()
            .unwrap()
            .version,
        version("3.0.0")
    );
    let rows = store
        .releases(&app("demo"), &Visibility::default(), None)
        .unwrap();
    assert_eq!(s.audit.records().unwrap().len(), rows.len());
    // Whatever interleaving happened, the stored versions strictly decrease newest first.
    assert!(rows.windows(2).all(|w| w[0].version > w[1].version));
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn eight_concurrent_commits_of_one_release_insert_once() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let r = source().publish("demo", "1.0.0", "", 12);
    let barrier = Arc::new(Barrier::new(9));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            let r = r.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.commit(r, publisher(12)).unwrap().inserted
            })
        })
        .collect();
    barrier.wait();
    let inserted = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter(|inserted| *inserted)
        .count();
    assert_eq!(inserted, 1);
    assert_eq!(store.digests().unwrap(), vec![r.digest]);
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn concurrent_removals_mark_once() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let r = source().publish("demo", "1.0.0", "", 12);
    store.commit(r.clone(), publisher(12)).unwrap();
    let barrier = Arc::new(Barrier::new(9));
    let threads: Vec<_> = (0..8_i64)
        .map(|i| {
            let store = store.clone();
            let r = r.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .remove(
                        &r.app,
                        &r.version,
                        "publisher12",
                        Removal {
                            at: i,
                            reason: format!("reason {i}"),
                        },
                        false,
                    )
                    .unwrap()
            })
        })
        .collect();
    barrier.wait();
    let marked: Vec<_> = threads
        .into_iter()
        .filter_map(|t| t.join().unwrap())
        .collect();
    assert_eq!(marked.len(), 1);
    let stored = store.release(&r.app, &r.version).unwrap().unwrap();
    assert_eq!(stored.yanked, marked[0].yanked);
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn concurrent_transfers_from_one_generation_apply_once() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let r = source().publish("demo", "1.0.0", "", 12);
    store.commit(r.clone(), publisher(12)).unwrap();
    let barrier = Arc::new(Barrier::new(5));
    let threads: Vec<_> = [13_u8, 14, 15, 16]
        .into_iter()
        .map(|to| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.transfer(transfer("demo", 12, to, 0), publisher(to))
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert_eq!(store.ownership(&r.app).unwrap().unwrap().generation, 1);
    assert_eq!(store.transfers(&r.app).unwrap().len(), 1);
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}
