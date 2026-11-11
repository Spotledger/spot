//! Tenant-capacity enforcement: the bound is checked inside the insert, so neither a single
//! caller nor a crowd of them can push a database past it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use spot_tenancy::{
    ClusterId, ControlStore, DatabaseId, Edition, MemoryStore, Placement, Region, Tenant,
    TenantFlags, TenantId, TenantName, TenantState, codes,
};

fn placement(cluster: &str, database: &str, id: TenantId) -> Placement {
    Placement {
        cluster: ClusterId::new(cluster).unwrap(),
        database: DatabaseId::new(database).unwrap(),
        schema: id.schema_name(),
    }
}

fn tenant(name: &str, cluster: &str, database: &str, seed: u8, now: u64) -> Tenant {
    let id = TenantId::new_v7(now, [seed; 10]);
    Tenant {
        id,
        name: TenantName::new(name).unwrap(),
        region: Region::new("eu-west").unwrap(),
        edition: Edition::new("standard").unwrap(),
        state: TenantState::Provisioning,
        placement: placement(cluster, database, id),
        created: now,
        flags: TenantFlags::NONE,
        version: 0,
        retention: None,
        name_reserved_until: None,
    }
}

#[test]
fn the_capacity_bound_is_checked_by_the_insert() {
    let store = MemoryStore::new();
    for seed in 0..3u8 {
        let t = tenant(&format!("tenant-{seed}"), "c1", "db1", seed, 1_000);
        store.insert_within(t, 1_000, 3).unwrap();
    }
    let extra = tenant("tenant-extra", "c1", "db1", 9, 1_001);
    let error = store.insert_within(extra, 1_001, 3).unwrap_err();
    assert_eq!(error.code(), codes::CAPACITY_EXCEEDED);
    assert_eq!(
        store
            .load()
            .unwrap()
            .get(&(
                ClusterId::new("c1").unwrap(),
                DatabaseId::new("db1").unwrap()
            ))
            .copied(),
        Some(3)
    );
    assert_eq!(store.list().unwrap().len(), 3);
}

#[test]
fn a_zero_capacity_database_admits_nobody() {
    let store = MemoryStore::new();
    let error = store
        .insert_within(tenant("tenant-a", "c1", "db1", 1, 1_000), 1_000, 0)
        .unwrap_err();
    assert_eq!(error.code(), codes::CAPACITY_EXCEEDED);
    assert_eq!(store.list().unwrap().len(), 0);
}

#[test]
fn capacity_is_per_database_and_deleted_tenants_do_not_count() {
    let store = MemoryStore::new();
    let first = store
        .insert_within(tenant("tenant-a", "c1", "db1", 1, 1_000), 1_000, 1)
        .unwrap();
    // Another database on the same cluster has its own bound.
    store
        .insert_within(tenant("tenant-b", "c1", "db2", 2, 1_000), 1_000, 1)
        .unwrap();
    // The full database refuses.
    assert_eq!(
        store
            .insert_within(tenant("tenant-c", "c1", "db1", 3, 1_001), 1_001, 1)
            .unwrap_err()
            .code(),
        codes::CAPACITY_EXCEEDED
    );
    // A deleted tenant frees its slot: it is gone, though its record stays for audit.
    let mut gone = first.clone();
    gone.state = TenantState::Deleted;
    store.update(gone, first.version).unwrap();
    store
        .insert_within(tenant("tenant-d", "c1", "db1", 4, 1_002), 1_002, 1)
        .unwrap();
    assert_eq!(store.list().unwrap().len(), 3);
}

#[test]
fn concurrent_inserts_never_exceed_the_capacity() {
    const THREADS: u8 = 16;
    const CAPACITY: u32 = 5;
    let store = Arc::new(MemoryStore::new());
    let winners = Arc::new(AtomicU32::new(0));
    let losers = Arc::new(AtomicU32::new(0));
    let mut handles = Vec::new();
    for seed in 0..THREADS {
        let store = Arc::clone(&store);
        let winners = Arc::clone(&winners);
        let losers = Arc::clone(&losers);
        handles.push(std::thread::spawn(move || {
            let t = tenant(&format!("tenant-{seed}"), "c1", "db1", seed, 1_000);
            match store.insert_within(t, 1_000, CAPACITY) {
                Ok(_) => {
                    winners.fetch_add(1, Ordering::SeqCst);
                }
                Err(error) => {
                    assert_eq!(error.code(), codes::CAPACITY_EXCEEDED);
                    losers.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(winners.load(Ordering::SeqCst), CAPACITY);
    assert_eq!(losers.load(Ordering::SeqCst), u32::from(THREADS) - CAPACITY);
    assert_eq!(
        store.list().unwrap().len(),
        usize::try_from(CAPACITY).unwrap()
    );
}

/// The licence-limit race: with one slot left, two callers released together must
/// produce exactly one winner and one refusal, every round. A check that is separate from the
/// save lets both pass.
#[test]
fn two_racing_inserts_at_limit_minus_one_admit_exactly_one() {
    const ROUNDS: u8 = 60;
    let store = Arc::new(MemoryStore::new());
    for round in 0..ROUNDS {
        // The database already holds `round` live tenants; the limit leaves one slot.
        let capacity = u32::from(round) + 1;
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2u8)
            .map(|side| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let seed = round * 2 + side;
                std::thread::spawn(move || {
                    let t = tenant(&format!("race-{seed}"), "c1", "db1", seed, 1_000);
                    barrier.wait();
                    store.insert_within(t, 1_000, capacity)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let wins = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            wins, 1,
            "round {round}: exactly one save may win the last slot"
        );
        for refused in results.iter().filter_map(|r| r.as_ref().err()) {
            assert_eq!(refused.code(), codes::CAPACITY_EXCEEDED);
        }
        assert_eq!(store.list().unwrap().len(), usize::from(round) + 1);
    }
}

/// The save is version-checked: two saves made on the same stored version, released together,
/// give exactly one winner and one version conflict, and the version moves by one.
#[test]
fn two_racing_saves_on_one_version_admit_exactly_one() {
    const ROUNDS: u8 = 40;
    for round in 0..ROUNDS {
        let store = Arc::new(MemoryStore::new());
        let stored = store
            .insert_within(tenant("alpha", "c1", "db1", round, 1_000), 1_000, 4)
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let mut next = stored.clone();
                next.state = TenantState::Active;
                std::thread::spawn(move || {
                    barrier.wait();
                    store.update(next, 1)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "round {round}"
        );
        for refused in results.iter().filter_map(|r| r.as_ref().err()) {
            assert_eq!(refused.code(), codes::VERSION_CONFLICT);
        }
        assert_eq!(store.get(&stored.id).unwrap().version, 2);
    }
}
