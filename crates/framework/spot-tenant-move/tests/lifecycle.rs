//! The production quiesce adapter against the real in-memory gate and hold service.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_core::SiteId;
use spot_effect_hold::{
    HoldService, InMemoryLedger, PolicyTable, RecordingAudit, RecordingDispatcher,
    RecordingNotifier,
};
use spot_migration_apply::{AdmissionGate, InMemoryAdmissionGate};
use spot_tenancy::{ClusterId, DatabaseId, Placement, Region, TenantId};
use spot_tenant_move::{LifecycleQuiesce, MoveId, MoveMode, MovePlan, QuiescePort};

fn plan() -> MovePlan {
    let tenant = TenantId::new_v7(1_700_000_000_000, [7; 10]);
    let placement = |cluster: &str, database: &str| Placement {
        cluster: ClusterId::new(cluster).unwrap(),
        database: DatabaseId::new(database).unwrap(),
        schema: tenant.schema_name(),
    };
    MovePlan::new(
        tenant,
        placement("a", "db1"),
        placement("b", "db2"),
        Region::new("eu-west").unwrap(),
        MoveMode::Offline,
    )
    .unwrap()
}

#[tokio::test]
async fn the_adapter_closes_the_gate_begins_the_hold_and_reopens_both_idempotently() {
    let site = SiteId::new("tenant-a").unwrap();
    let gate = Arc::new(InMemoryAdmissionGate::new());
    let hold = Arc::new(HoldService::new(
        site,
        Arc::new(InMemoryLedger::new()),
        Arc::new(PolicyTable::default()),
        Arc::new(RecordingAudit::new()),
        Arc::new(RecordingDispatcher::new()),
        Arc::new(RecordingNotifier::new()),
    ));
    let adapter = LifecycleQuiesce::new(gate.clone(), hold);
    let plan = plan();
    let id = MoveId::new("move-01").unwrap();

    adapter.quiesce(&plan, &id, 1).await.unwrap();
    assert!(!gate.status().is_open());
    // A retried step after a crash finds the same live hold and does not fail.
    adapter.quiesce(&plan, &id, 2).await.unwrap();
    assert!(!gate.status().is_open());

    adapter.resume(&plan, &id, 3).await.unwrap();
    assert!(gate.status().is_open());
    // A retried cleanup after the hold was released does not fail.
    adapter.resume(&plan, &id, 4).await.unwrap();
    assert!(gate.status().is_open());
}
