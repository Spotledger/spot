//! The tenant-capacity bound on PostgreSQL: the count and the insert are one locked
//! transaction, so concurrent control nodes can never exceed it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicU32, Ordering};

use common::{fixture, runtime};
use spot_tenancy::{
    ClusterId, ControlStore, DatabaseId, Edition, Placement, Region, Tenant, TenantFlags, TenantId,
    TenantName, TenantState, codes,
};

fn tenant(cluster: &ClusterId, database: &DatabaseId, name: &str, seed: u8, now: u64) -> Tenant {
    let id = TenantId::new_v7(now, [seed; 10]);
    Tenant {
        id,
        name: TenantName::new(name).unwrap(),
        region: Region::new("eu-west").unwrap(),
        edition: Edition::new("standard").unwrap(),
        state: TenantState::Provisioning,
        placement: Placement {
            cluster: cluster.clone(),
            database: database.clone(),
            schema: id.schema_name(),
        },
        created: now,
        flags: TenantFlags::NONE,
        version: 0,
        retention: None,
        name_reserved_until: None,
    }
}

#[test]
fn concurrent_inserts_never_exceed_the_capacity() {
    const THREADS: u8 = 8;
    const CAPACITY: u32 = 3;
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let store = &f.store;
    let cluster = f.cluster.clone();
    let database = f.database.clone();
    let winners = AtomicU32::new(0);
    let losers = AtomicU32::new(0);
    std::thread::scope(|scope| {
        for seed in 0..THREADS {
            let cluster = cluster.clone();
            let database = database.clone();
            let winners = &winners;
            let losers = &losers;
            scope.spawn(move || {
                let candidate = tenant(&cluster, &database, &format!("tenant-{seed}"), seed, 1_000);
                match store.insert_within(candidate, 1_000, CAPACITY) {
                    Ok(_) => {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(error) => {
                        assert_eq!(error.code(), codes::CAPACITY_EXCEEDED);
                        losers.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
        }
    });
    assert_eq!(winners.load(Ordering::SeqCst), CAPACITY);
    assert_eq!(losers.load(Ordering::SeqCst), u32::from(THREADS) - CAPACITY);
    assert_eq!(
        store.list().unwrap().len(),
        usize::try_from(CAPACITY).unwrap()
    );

    let stored = rt.block_on(async {
        let client = f.db.raw_client().await;
        client
            .query_one(
                "SELECT count(*) FROM spot_control.tenant \
                 WHERE cluster = $1 AND database = $2 AND state <> 'deleted'",
                &[&cluster.as_str(), &database.as_str()],
            )
            .await
            .expect("count the control rows")
            .get::<_, i64>(0)
    });
    assert_eq!(stored, i64::from(CAPACITY));
}

#[test]
fn a_refused_insert_leaves_nothing_behind() {
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let first = tenant(&f.cluster, &f.database, "tenant-first", 1, 1_000);
    let stored = f.store.insert_within(first, 1_000, 1).unwrap();
    assert_eq!(
        f.store
            .insert_within(
                tenant(&f.cluster, &f.database, "tenant-late", 2, 1_001),
                1_001,
                1
            )
            .unwrap_err()
            .code(),
        codes::CAPACITY_EXCEEDED
    );
    // The refused row was never committed: the name is still free once a slot opens.
    let mut gone = stored.clone();
    gone.state = TenantState::Deleted;
    f.store.update(gone, stored.version).unwrap();
    f.store
        .insert_within(
            tenant(&f.cluster, &f.database, "tenant-late", 2, 1_002),
            1_002,
            1,
        )
        .unwrap();
}

/// The licence-limit race on PostgreSQL: with one slot left, two callers released
/// together give exactly one winner and one refusal, round after round.
#[test]
fn two_racing_inserts_at_limit_minus_one_admit_exactly_one() {
    const ROUNDS: u8 = 20;
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let store = &f.store;
    for round in 0..ROUNDS {
        let capacity = u32::from(round) + 1;
        let barrier = std::sync::Barrier::new(2);
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2u8)
                .map(|side| {
                    let seed = round * 2 + side;
                    let candidate = tenant(
                        &f.cluster,
                        &f.database,
                        &format!("race-{seed}"),
                        seed,
                        1_000,
                    );
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        store.insert_within(candidate, 1_000, capacity)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "round {round}: exactly one save may win the last slot"
        );
        for refused in results.iter().filter_map(|r| r.as_ref().err()) {
            assert_eq!(refused.code(), codes::CAPACITY_EXCEEDED);
        }
        assert_eq!(store.list().unwrap().len(), usize::from(round) + 1);
    }
}

/// The save is version-checked on PostgreSQL: two saves on one stored version give exactly one
/// winner and one version conflict.
#[test]
fn two_racing_saves_on_one_version_admit_exactly_one() {
    const ROUNDS: u8 = 10;
    let rt = runtime();
    let Some(f) = rt.block_on(fixture(rt.clone())) else {
        return;
    };
    let store = &f.store;
    for round in 0..ROUNDS {
        let stored = store
            .insert_within(
                tenant(
                    &f.cluster,
                    &f.database,
                    &format!("versioned-{round}"),
                    round,
                    1_000,
                ),
                1_000,
                u32::MAX,
            )
            .unwrap();
        let barrier = std::sync::Barrier::new(2);
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let mut next = stored.clone();
                    next.state = TenantState::Active;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        store.update(next, stored.version)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "round {round}"
        );
        for refused in results.iter().filter_map(|r| r.as_ref().err()) {
            assert_eq!(refused.code(), codes::VERSION_CONFLICT);
        }
        assert_eq!(store.get(&stored.id).unwrap().version, stored.version + 1);
    }
}
