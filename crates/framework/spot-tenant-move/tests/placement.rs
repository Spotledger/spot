//! The production placement adapter: the control store flip and the routing-cache
//! invalidation, against the in-memory control store.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use spot_tenancy::{
    ClusterId, ControlStore, DatabaseId, Edition, MemoryStore, Placement, Region, Tenant,
    TenantFlags, TenantId, TenantName, TenantState,
};
use spot_tenant_move::{
    ControlStorePlacement, MoveMode, MovePlan, PlacementPort, RoutingCacheInvalidator,
};

struct RecordingCache {
    calls: AtomicU32,
    tenants: Mutex<Vec<TenantId>>,
}

impl RecordingCache {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
            tenants: Mutex::new(Vec::new()),
        }
    }
}

impl RoutingCacheInvalidator for RecordingCache {
    fn invalidate(&self, tenant: &TenantId) -> Result<(), &'static str> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.tenants.lock().unwrap().push(*tenant);
        Ok(())
    }
}

fn tenant() -> TenantId {
    TenantId::new_v7(1_700_000_000_000, [7; 10])
}

fn placement(cluster: &str, database: &str) -> Placement {
    Placement {
        cluster: ClusterId::new(cluster).unwrap(),
        database: DatabaseId::new(database).unwrap(),
        schema: tenant().schema_name(),
    }
}

fn plan() -> MovePlan {
    MovePlan::new(
        tenant(),
        placement("a", "db1"),
        placement("b", "db2"),
        Region::new("eu-west").unwrap(),
        MoveMode::Offline,
    )
    .unwrap()
}

fn record(plan: &MovePlan) -> Tenant {
    Tenant {
        id: plan.tenant,
        name: TenantName::new("acme").unwrap(),
        region: Region::new("eu-west").unwrap(),
        edition: Edition::new("standard").unwrap(),
        state: TenantState::Provisioning,
        placement: plan.from.clone(),
        created: 1,
        flags: TenantFlags::NONE,
        version: 0,
        retention: None,
        name_reserved_until: None,
    }
}

#[tokio::test]
async fn cut_over_flips_the_placement_and_restore_puts_it_back() {
    let store = Arc::new(MemoryStore::new());
    let plan = plan();
    store.insert_within(record(&plan), 1, u32::MAX).unwrap();
    let mut active = store.get(&plan.tenant).unwrap();
    active.state = TenantState::Active;
    store.update(active, 1).unwrap();
    let cache = Arc::new(RecordingCache::new());
    let adapter = ControlStorePlacement::new(store.clone(), cache.clone());

    adapter.cut_over(&plan, 1).await.unwrap();
    let stored = store.get(&plan.tenant).unwrap();
    assert_eq!(stored.placement, plan.to);
    assert_eq!(stored.state, TenantState::Active);
    assert_eq!(cache.calls.load(Ordering::SeqCst), 1);
    assert_eq!(cache.tenants.lock().unwrap()[0], plan.tenant);

    // A retried cut-over is a no-op but refreshes the caches.
    adapter.cut_over(&plan, 2).await.unwrap();
    assert_eq!(cache.calls.load(Ordering::SeqCst), 2);

    adapter.restore(&plan).await.unwrap();
    let stored = store.get(&plan.tenant).unwrap();
    assert_eq!(stored.placement, plan.from);
    assert_eq!(stored.state, TenantState::Active);
    assert_eq!(cache.calls.load(Ordering::SeqCst), 3);
}
