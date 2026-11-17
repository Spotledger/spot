//! The parking store, repair list, checkpoints, state and catalog on a real PostgreSQL, and the
//! intake scenarios (park, release, replay, restart mid-park, holds) run over the PG stores.
//! Gated on `SPOT_TEST_PG_URL` (skipped with a message when unset, a failure when
//! `SPOT_REQUIRE_PG=1`); the statement inspection needs no server.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use common::{TENANT, World, bundle, hub_source, put, world};
use spot_storage_contract::RecordId;
use spot_sync_closure::{ClosureError, RecordKey, References};
use spot_sync_node::{
    Delivery, NodeConfig, NodeIntake, ParkedBundle, ParkingLimits, ParkingStore, Receipt,
    RepairCause, RepairItem,
};
use spot_sync_node_pg::{
    EntityInfo, Generation, PgApplyRouter, PgCheckpoints, PgLedger, PgNodeCatalog, PgNodeError,
    PgNodeState, PgParkingStore, PgRepairList, STATEMENTS,
};
use spot_sync_protocol::{Checkpoint, CheckpointStore};
use spot_sync_skew::{HoldReason, HoldSet, NodeReport, RecordRef, SoftwareVersion, on_undecodable};

#[test]
fn every_statement_on_a_node_table_carries_the_tenant() {
    assert!(STATEMENTS.len() > 20);
    for sql in STATEMENTS {
        let by_tenant = sql.contains("tenant = $1")
            || (sql.trim_start().starts_with("INSERT") && sql.contains("(tenant,"));
        assert!(by_tenant, "no tenant predicate: {sql}");
    }
}

fn parked(tenant: &str, seq: u64, first: u64, missing: &[RecordKey]) -> ParkedBundle {
    let b = bundle(vec![put(RecordId::new_v7(), "p", first)]);
    ParkedBundle {
        seq,
        tenant: tenant.to_owned(),
        bytes: b.bytes,
        bundle: b,
        missing: missing.iter().cloned().collect::<BTreeSet<_>>(),
        parked_at_ms: 5,
        first_position: first,
        serve: None,
    }
}

fn key(n: &str) -> RecordKey {
    RecordKey::new("item", n)
}

#[test]
fn parking_keeps_arrival_order_across_a_restart_and_never_crosses_tenants() {
    let Some(w) = world() else { return };
    let mut store = PgParkingStore::new(w.db.clone());
    // The intake's `seq` restarts from zero in every process: the order is the arrival order.
    store.park(parked("a", 7, 10, &[key("x")]));
    store.park(parked("a", 0, 11, &[key("x"), key("y")]));
    store.park(parked("b", 0, 12, &[key("x")]));
    assert_eq!((store.len("a"), store.len("b")), (2, 1));
    assert!(store.bytes("a") > 0);
    // A new store on the same database sees the same queue (a restart in the middle of a park).
    let mut again = PgParkingStore::new(w.db.clone());
    assert_eq!(again.len("a"), 2);
    // `x` arrives: the first bundle is released, the second still misses `y`; tenant b is
    // another queue and is untouched by tenant a's keys.
    let ready = again.ready("a", &[key("x")]);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].first_position, 10);
    assert_eq!(again.len("a"), 1);
    assert_eq!(again.len("b"), 1);
    let ready = again.ready("a", &[key("y")]);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].first_position, 11);
    assert!(again.is_empty("a"));
    // The oldest of a tenant comes out first.
    again.park(parked("c", 0, 20, &[key("z")]));
    again.park(parked("c", 1, 21, &[key("z")]));
    assert_eq!(again.take_oldest("c").unwrap().first_position, 20);
    assert_eq!(again.take_oldest("c").unwrap().first_position, 21);
    assert!(again.take_oldest("c").is_none());
    assert!(again.take_failure().is_none());
}

#[test]
fn expiry_uses_the_database_clock_and_visits_every_tenant() {
    let Some(w) = world() else { return };
    let mut store = PgParkingStore::new(w.db.clone());
    store.park(parked("a", 0, 1, &[key("x")]));
    store.park(parked("b", 0, 2, &[key("x")]));
    // Nothing is older than an hour yet.
    assert!(store.expire(0, 3_600_000).is_empty());
    w.exec("UPDATE spot_sync_node_parked SET parked_at = now() - interval '2 hours'");
    let expired = store.expire(0, 3_600_000);
    assert_eq!(expired.len(), 2);
    assert!(store.is_empty("a") && store.is_empty("b"));
}

#[test]
fn the_repair_list_and_the_recomputed_prefix() {
    let Some(w) = world() else { return };
    let mut apply = w.apply(TENANT);
    let source = hub_source();
    use spot_sync_node::BundleApply;
    for position in 1..=5_u64 {
        apply
            .apply_bundle(
                TENANT,
                &bundle(vec![put(RecordId::new_v7(), "r", position)]),
            )
            .unwrap();
    }
    let ledger = PgLedger::new(w.db.clone());
    let state = PgNodeState::new(w.db.clone());
    assert_eq!(
        ledger.max_position(TENANT, source.as_str()).unwrap(),
        Some(5)
    );
    assert_eq!(
        state.applied_prefix(TENANT, source.as_str()).unwrap(),
        Some(5)
    );
    // A parked bundle whose first position is 3 holds the prefix at 2.
    let mut store = PgParkingStore::new(w.db.clone());
    store.park(parked(TENANT, 0, 3, &[key("x")]));
    assert_eq!(
        state.applied_prefix(TENANT, source.as_str()).unwrap(),
        Some(2)
    );
    // Pushed out of parking onto the repair list, it still holds the prefix.
    let out = store.take_oldest(TENANT).unwrap();
    let repair = PgRepairList::new(w.db.clone());
    repair
        .push(&[RepairItem {
            tenant: TENANT.to_owned(),
            first_position: out.first_position,
            missing: vec![key("x")],
            cause: RepairCause::Bound,
        }])
        .unwrap();
    assert_eq!(
        state.applied_prefix(TENANT, source.as_str()).unwrap(),
        Some(2)
    );
    let listed = repair.list(TENANT, 10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].item.cause, RepairCause::Bound);
    assert_eq!(repair.len("other").unwrap(), 0);
    // Repaired: the prefix is the highest applied position again.
    assert!(repair.remove(TENANT, listed[0].id).unwrap());
    assert!(repair.is_empty(TENANT).unwrap());
    assert_eq!(
        state.applied_prefix(TENANT, source.as_str()).unwrap(),
        Some(5)
    );
}

#[test]
fn a_checkpoint_goes_forward_only_and_never_above_the_ledger() {
    let Some(w) = world() else { return };
    use spot_sync_node::BundleApply;
    let source = hub_source();
    let mut apply = w.apply(TENANT);
    for position in 1..=4_u64 {
        apply
            .apply_bundle(
                TENANT,
                &bundle(vec![put(RecordId::new_v7(), "r", position)]),
            )
            .unwrap();
    }
    let mut checkpoints = PgCheckpoints::new(w.db.clone(), TENANT).unwrap();
    assert!(checkpoints.load(&source).is_none());
    // Ahead of the ledger: refused, nothing stored.
    assert!(
        checkpoints
            .store(&Checkpoint::new(source.clone(), 5))
            .is_err()
    );
    assert!(checkpoints.load(&source).is_none());
    checkpoints
        .store(&Checkpoint::new(source.clone(), 3))
        .unwrap();
    // Never backward.
    checkpoints
        .store(&Checkpoint::new(source.clone(), 2))
        .unwrap();
    assert_eq!(checkpoints.load(&source).unwrap().last_applied_seq, 3);
    checkpoints
        .store(&Checkpoint::new(source.clone(), 4))
        .unwrap();
    assert_eq!(checkpoints.load(&source).unwrap().last_applied_seq, 4);
    // Another tenant has its own.
    let other = PgCheckpoints::new(w.db.clone(), "other").unwrap();
    assert!(other.load(&source).is_none());
}

#[test]
fn holds_and_horizons_round_trip_and_a_damaged_value_is_refused_whole() {
    let Some(w) = world() else { return };
    let state = PgNodeState::new(w.db.clone());
    assert!(state.load_holds(TENANT).unwrap().is_none());
    let mut holds = HoldSet::new();
    for (id, version, required) in [("b", 4, 9), ("a", 7, 8)] {
        holds.insert(on_undecodable(
            RecordRef {
                entity: "item".to_owned(),
                id: id.to_owned(),
                version,
            },
            HoldReason::MetadataTooLow { required },
        ));
    }
    state.save_holds(TENANT, &holds).unwrap();
    assert_eq!(state.load_holds(TENANT).unwrap().unwrap(), holds);
    // Another tenant sees none.
    assert!(state.load_holds("other").unwrap().is_none());
    let mut horizons = BTreeMap::new();
    horizons.insert("mandatory".to_owned(), 41_u64);
    state.save_horizons(TENANT, &horizons).unwrap();
    assert_eq!(state.load_horizons(TENANT).unwrap(), horizons);
    // A damaged stored hold set is refused as a whole.
    w.exec("UPDATE spot_sync_node_state SET value = '{\"holds\": 3' WHERE kind = 'holds'");
    assert_eq!(state.load_holds(TENANT), Err(PgNodeError::StateInvalid));
}

#[test]
fn building_off_a_runtime_thread_and_missing_tables_are_refused() {
    let Some(w) = world() else { return };
    w.db.require_tables().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let refused = rt.block_on(async { spot_sync_node_pg::Db::new(w.bridge.clone(), common::POOL) });
    assert_eq!(
        refused.unwrap_err().code(),
        "sync_node_pg.unsupported_context"
    );
    w.exec("DROP TABLE spot_sync_node_repair");
    let missing = w.db.require_tables().unwrap_err();
    assert_eq!(missing.code(), "sync_node_pg.tables_missing");
    w.db.prepare().unwrap();
    w.db.require_tables().unwrap();
}

// ---- the intake over the PG stores ------------------------------------------------------------

/// The links of the test records, by key.
struct MapRefs(BTreeMap<String, Vec<RecordKey>>);

impl References for MapRefs {
    fn references(
        &self,
        _entity: &str,
        image: &serde_json::Value,
    ) -> Result<Vec<RecordKey>, ClosureError> {
        let id = image["document"]["id"].as_str().unwrap_or_default();
        Ok(self.0.get(id).cloned().unwrap_or_default())
    }
}

fn generation(links: BTreeMap<String, Vec<RecordKey>>, version: u32) -> Generation {
    let mut entities = BTreeMap::new();
    entities.insert(
        "item".to_owned(),
        EntityInfo {
            topic: "item_topic".to_owned(),
            major: 1,
        },
    );
    Generation {
        version,
        references: Arc::new(MapRefs(links)),
        entities,
    }
}

fn report(metadata_version: u32) -> NodeReport {
    let mut majors = BTreeMap::new();
    majors.insert("item_topic".to_owned(), BTreeSet::from([1_u32]));
    NodeReport {
        software_version: SoftwareVersion::new(1, 0, 0),
        metadata_version,
        migration_epoch: 1,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: majors,
    }
}

type PgIntake = NodeIntake<PgApplyRouter, PgParkingStore, PgNodeCatalog>;

fn intake(w: &World, catalog: PgNodeCatalog) -> PgIntake {
    let mut router = PgApplyRouter::new();
    router.insert(w.apply(TENANT));
    NodeIntake::new(
        NodeConfig {
            hub_source: hub_source(),
            limits: ParkingLimits::default(),
            max_tenants: 4,
        },
        router,
        PgParkingStore::new(w.db.clone()),
        catalog,
        report(1),
    )
    .unwrap()
}

fn delivery(records: Vec<spot_sync_closure::BundleRecord>) -> Delivery {
    Delivery {
        source: hub_source(),
        tenant: TENANT.to_owned(),
        bundle: bundle(records),
    }
}

#[test]
fn the_intake_parks_releases_replays_and_survives_a_restart_mid_park() {
    let Some(w) = world() else { return };
    let (parent, child, later) = (RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7());
    let mut links = BTreeMap::new();
    links.insert(
        child.to_string(),
        vec![RecordKey::new("item", parent.to_string())],
    );
    links.insert(
        later.to_string(),
        vec![RecordKey::new("item", parent.to_string())],
    );
    let catalog = PgNodeCatalog::new(generation(links, 1));
    let mut first = intake(&w, catalog.clone());
    // The child references a parent the node lacks: the whole bundle is parked.
    match first
        .receive(delivery(vec![put(child, "child", 2)]), 1)
        .unwrap()
    {
        Receipt::Parked { missing } => assert_eq!(missing.len(), 1),
        other => panic!("expected a park, got {other:?}"),
    }
    assert_eq!(first.parking().len(TENANT), 1);
    assert!(w.stored(child).is_none());
    // The process restarts: a new intake and a new parking store over the same tables.
    drop(first);
    let mut second = intake(&w, catalog.clone());
    assert_eq!(second.parking().len(TENANT), 1);
    // The parent arrives: it is applied and releases the parked child.
    match second
        .receive(delivery(vec![put(parent, "parent", 1)]), 2)
        .unwrap()
    {
        Receipt::Applied(applied) => {
            assert_eq!(applied.applied.records.len(), 1);
            assert_eq!(applied.released.len(), 1);
        }
        other => panic!("expected an apply, got {other:?}"),
    }
    assert!(second.parking().is_empty(TENANT));
    assert_eq!(w.stored(child).unwrap().1, "child");
    // A replay of the parent is counted, not applied again.
    match second
        .receive(delivery(vec![put(parent, "parent", 1)]), 3)
        .unwrap()
    {
        Receipt::Applied(applied) => {
            assert_eq!(applied.duplicates, 1);
            assert!(applied.applied.records.is_empty());
        }
        other => panic!("expected a replay, got {other:?}"),
    }
    // A bundle whose reference is satisfied applies at once.
    assert!(matches!(
        second
            .receive(delivery(vec![put(later, "later", 3)]), 4)
            .unwrap(),
        Receipt::Applied(_)
    ));
}

#[test]
fn a_record_of_an_unknown_entity_is_held_until_a_metadata_swap() {
    let Some(w) = world() else { return };
    let id = RecordId::new_v7();
    let mut unknown = put(id, "x", 1);
    unknown.key = RecordKey::new("not_in_catalog", id.to_string());
    let catalog = PgNodeCatalog::new(generation(BTreeMap::new(), 1));
    let mut node = intake(&w, catalog.clone());
    match node.receive(delivery(vec![unknown]), 1).unwrap() {
        Receipt::Held(_) => {}
        other => panic!("expected a hold, got {other:?}"),
    }
    assert!(w.stored(id).is_none());
    // The hold is saved in its golden form and read back.
    let holds = node.holds(TENANT).unwrap().clone();
    assert!(!holds.is_empty());
    let state = PgNodeState::new(w.db.clone());
    state.save_holds(TENANT, &holds).unwrap();
    assert_eq!(state.load_holds(TENANT).unwrap().unwrap(), holds);
    // A swap to a newer generation changes what the node can decode.
    catalog.swap(generation(BTreeMap::new(), 2));
    assert_eq!(catalog.version(), 2);
}
