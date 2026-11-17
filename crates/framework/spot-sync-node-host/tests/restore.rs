//! The durable intake (WP-RTB-02.T12): a node stopped at any point of a receive acknowledges
//! nothing it has not applied, loses no hold and no repair entry, and the hub's redelivery brings
//! it to the state it would have reached without the stop.
//!
//! The stores are in-memory doubles that survive the "restart" (the intake is dropped and built
//! again over them); the PostgreSQL stores behind the same traits are exercised by
//! `spot-cli`'s `tests/sync_node_intake.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::sync::{Arc, Mutex};

use spot_sync_closure::RecordKey;
use spot_sync_node::{
    HoldSet, MemoryParking, NodeConfig, NodeIntake, ParkedBundle, ParkingLimits, ParkingStore,
    RepairItem, RestoreState,
};
use spot_sync_node_host::envelope::Served;
use spot_sync_node_host::intake::{DurableIntake, Failpoints, NOT_RESTORED, Point, StateStore};
use spot_sync_node_host::{Error, Intake, Result};
use support::*;

#[derive(Clone, Default)]
struct SharedParking(Arc<Mutex<MemoryParking>>);

impl ParkingStore for SharedParking {
    fn park(&mut self, parked: ParkedBundle) {
        self.0.lock().unwrap().park(parked);
    }
    fn ready(&mut self, tenant: &str, applied_keys: &[RecordKey]) -> Vec<ParkedBundle> {
        self.0.lock().unwrap().ready(tenant, applied_keys)
    }
    fn take_oldest(&mut self, tenant: &str) -> Option<ParkedBundle> {
        self.0.lock().unwrap().take_oldest(tenant)
    }
    fn len(&self, tenant: &str) -> usize {
        self.0.lock().unwrap().len(tenant)
    }
    fn bytes(&self, tenant: &str) -> usize {
        self.0.lock().unwrap().bytes(tenant)
    }
    fn expire(&mut self, now_ms: u64, max_age_ms: u64) -> Vec<ParkedBundle> {
        self.0.lock().unwrap().expire(now_ms, max_age_ms)
    }
}

#[derive(Default)]
struct Saved {
    holds: Option<HoldSet>,
    repair: Vec<RepairItem>,
    fail_push: bool,
    fail_load: bool,
}

/// The durable restore state, as the PostgreSQL store computes it: the applied prefix is the
/// highest applied position below every parked and repair-listed bundle.
#[derive(Clone, Default)]
struct MemState {
    saved: Arc<Mutex<Saved>>,
    apply: SharedApply,
    parking: SharedParking,
}

impl StateStore for MemState {
    fn load(&mut self, tenant: &str) -> Result<RestoreState> {
        let saved = self.saved.lock().unwrap();
        if saved.fail_load {
            return Err(Error::store_failed("the state could not be read"));
        }
        let parked: Vec<u64> = self
            .parking
            .0
            .lock()
            .unwrap()
            .parked(tenant)
            .iter()
            .map(|p| p.first_position)
            .collect();
        let floor = parked
            .iter()
            .copied()
            .chain(saved.repair.iter().map(|r| r.first_position))
            .min();
        let applied: Vec<u64> = self
            .apply
            .0
            .lock()
            .unwrap()
            .stored
            .iter()
            .filter(|((t, _), _)| t == tenant)
            .map(|(_, position)| *position)
            .collect();
        let applied_prefix = applied
            .into_iter()
            .filter(|position| floor.is_none_or(|f| *position < f))
            .max();
        Ok(RestoreState {
            applied_prefix,
            served_prefix: None,
            holds: saved.holds.clone(),
            parked_first_positions: parked,
            repair: saved.repair.clone(),
        })
    }

    fn save_holds(&mut self, _tenant: &str, holds: &HoldSet) -> Result<()> {
        self.saved.lock().unwrap().holds = Some(holds.clone());
        Ok(())
    }

    fn push_repair(&mut self, items: &[RepairItem]) -> Result<()> {
        let mut saved = self.saved.lock().unwrap();
        if saved.fail_push {
            return Err(Error::store_failed("the repair list could not be written"));
        }
        saved.repair.extend(items.iter().cloned());
        Ok(())
    }
}

struct Crash(Point);

impl Failpoints for Crash {
    fn crash_at(&self, point: Point) -> bool {
        point == self.0
    }
}

type Durable = DurableIntake<SharedApply, SharedParking, JsonCatalog, MemCursor, MemState>;

/// The durable world: what survives a restart.
#[derive(Clone, Default)]
struct World {
    apply: SharedApply,
    parking: SharedParking,
    cursor: MemCursor,
    state: MemState,
}

impl World {
    fn new() -> Self {
        let apply = SharedApply::default();
        let parking = SharedParking::default();
        Self {
            state: MemState {
                saved: Arc::default(),
                apply: apply.clone(),
                parking: parking.clone(),
            },
            apply,
            parking,
            cursor: MemCursor::default(),
        }
    }

    /// A process start: a fresh intake over the durable stores.
    fn start(&self, crash: Option<Point>) -> Result<Durable> {
        let config = NodeConfig {
            hub_source: hub_source(),
            limits: ParkingLimits::default(),
            max_tenants: 4,
        };
        let node = NodeIntake::new(
            config,
            self.apply.clone(),
            self.parking.clone(),
            JsonCatalog,
            report(),
        )
        .expect("valid config");
        let durable = DurableIntake::restore(
            node,
            self.cursor.clone(),
            self.state.clone(),
            TENANT,
            hub_source(),
        )?;
        Ok(match crash {
            Some(point) => durable.with_failpoints(Arc::new(Crash(point))),
            None => durable,
        })
    }

    fn cursor_value(&self) -> u64 {
        *self.cursor.0.lock().unwrap()
    }

    fn has(&self, entity: &str, id: &str) -> bool {
        self.apply
            .0
            .lock()
            .unwrap()
            .stored
            .contains_key(&(TENANT.to_owned(), RecordKey::new(entity, id)))
    }
}

/// bundle 1 applies, bundle 2 needs `Customer/c1` (parked), bundle 3 applies.
fn batch_one() -> Vec<Served> {
    vec![
        served(1, 0, &bundle("Customer", "c0", 5, &[])),
        served(2, 1, &bundle("Invoice", "i1", 6, &[("Customer", "c1")])),
        served(3, 2, &bundle("Customer", "c2", 7, &[])),
    ]
}

/// The bundle that brings the record bundle 2 waits for.
fn batch_two() -> Vec<Served> {
    vec![served(4, 3, &bundle("Customer", "c1", 8, &[]))]
}

/// What the hub does after a restart: it serves again everything above the stored cursor.
fn redeliver(items: Vec<Served>, cursor: u64) -> Vec<Served> {
    items.into_iter().filter(|s| s.stamp.seq > cursor).collect()
}

/// Every bundle at or below the acknowledged sequence is applied (the invariant of ADR-065).
fn acknowledged_is_applied(world: &World, items: &[Served]) {
    let cursor = world.cursor_value();
    for served in items.iter().filter(|s| s.stamp.seq <= cursor) {
        for record in &served.bundle.records {
            assert!(
                world.has(&record.key.entity, &record.key.id),
                "serve sequence {} is acknowledged but {}/{} is not applied",
                served.stamp.seq,
                record.key.entity,
                record.key.id
            );
        }
    }
}

#[test]
fn a_stop_at_every_point_of_a_receive_acknowledges_nothing_unapplied() {
    let points = [
        Point::AfterBundle(0),
        Point::AfterBundle(1),
        Point::AfterBundle(2),
        Point::AfterReceive,
        Point::AfterHolds,
        Point::AfterRepair,
    ];
    for point in points {
        let world = World::new();
        let mut first = world.start(Some(point)).unwrap();
        let stopped = first.receive(batch_one(), 0, 1_000);
        assert!(stopped.is_err(), "{point:?} stops the call");
        acknowledged_is_applied(&world, &batch_one());
        assert!(world.cursor_value() <= 1, "{point:?}: bundle 2 is parked");

        // The stopped intake acknowledges nothing more.
        assert_eq!(first.cursor().unwrap_err().code, NOT_RESTORED);
        assert_eq!(
            first.receive(Vec::new(), 0, 1_001).unwrap_err().code,
            NOT_RESTORED
        );

        // The restart restores, and the hub serves again what is above the stored cursor.
        let mut second = world.start(None).unwrap();
        assert_eq!(second.cursor().unwrap(), world.cursor_value());
        if world.parking.0.lock().unwrap().parked(TENANT).is_empty() {
            // Nothing is parked, so nothing holds the journal prefix.
        } else {
            let prefix = second.intake().applied_prefix(TENANT);
            assert!(
                prefix.is_none_or(|p| p < 6),
                "{point:?}: a parked bundle holds the prefix, got {prefix:?}"
            );
        }
        let again = redeliver(batch_one(), world.cursor_value());
        second.receive(again, 0, 2_000).unwrap();
        acknowledged_is_applied(&world, &batch_one());
        assert!(world.cursor_value() <= 1, "{point:?}: still waiting for c1");

        let done = second.receive(batch_two(), 0, 3_000).unwrap();
        assert_eq!(done.prefix, 4, "{point:?}");
        assert_eq!(world.cursor_value(), 4);
        for (entity, id) in [
            ("Customer", "c0"),
            ("Invoice", "i1"),
            ("Customer", "c2"),
            ("Customer", "c1"),
        ] {
            assert!(world.has(entity, id), "{point:?}: {entity}/{id}");
        }
    }
}

#[test]
fn without_a_stop_the_same_batches_end_in_the_same_state() {
    let world = World::new();
    let mut intake = world.start(None).unwrap();
    let first = intake.receive(batch_one(), 0, 1_000).unwrap();
    assert_eq!(first.prefix, 1);
    let second = intake.receive(batch_two(), 0, 2_000).unwrap();
    assert_eq!(second.prefix, 4);
    assert_eq!(world.cursor_value(), 4);
}

#[test]
fn holds_saved_after_a_receive_survive_a_restart() {
    let world = World::new();
    let mut intake = world.start(None).unwrap();
    let received = intake
        .receive(
            vec![
                served(1, 0, &bundle_major("Customer", "new", 5, 9)),
                served(2, 1, &bundle("Customer", "c2", 6, &[])),
            ],
            0,
            1_000,
        )
        .unwrap();
    assert_eq!((received.held, received.applied), (1, 1));
    let saved = world.state.saved.lock().unwrap().holds.clone();
    let saved = saved.expect("holds were saved after the receive");
    assert!(!saved.holds().is_empty());

    let restarted = world.start(None).unwrap();
    assert_eq!(restarted.intake().holds(TENANT), Some(&saved));
}

#[test]
fn a_stop_after_the_holds_were_saved_keeps_them_and_resends_the_bundle() {
    let world = World::new();
    let mut intake = world.start(Some(Point::AfterHolds)).unwrap();
    let items = vec![served(1, 0, &bundle_major("Customer", "new", 5, 9))];
    assert!(intake.receive(items.clone(), 0, 1_000).is_err());
    // The hold is durable, the cursor did not move: the hub serves the bundle again.
    assert!(world.state.saved.lock().unwrap().holds.is_some());
    assert_eq!(world.cursor_value(), 0);
    let mut restarted = world.start(None).unwrap();
    assert!(restarted.intake().holds(TENANT).is_some());
    let again = restarted.receive(items, 0, 2_000).unwrap();
    assert_eq!((again.held, again.prefix), (1, 1));
}

/// A parked bundle past its age goes to the repair list.
const PAST_THE_AGE: u64 = 24 * 60 * 60 * 1000 + 5_000;

#[test]
fn a_repair_entry_is_stored_before_the_cursor_and_holds_the_prefix_after_a_restart() {
    let world = World::new();
    let mut intake = world.start(None).unwrap();
    intake.receive(batch_one(), 0, 1_000).unwrap();
    // The next call finds bundle 2 older than the parking limit: it goes to the repair list.
    intake.receive(Vec::new(), 0, PAST_THE_AGE).unwrap();
    let repair = world.state.saved.lock().unwrap().repair.clone();
    assert_eq!(repair.len(), 1);
    assert_eq!(repair[0].first_position, 6);
    assert!(world.parking.0.lock().unwrap().parked(TENANT).is_empty());
    assert!(world.cursor_value() <= 1);

    // Restarted: the repair entry holds the journal prefix below bundle 2, and the hub's
    // redelivery (bundle 2, then the record it waits for) settles it.
    let mut restarted = world.start(None).unwrap();
    assert_eq!(restarted.intake().applied_prefix(TENANT), Some(5));
    assert_eq!(restarted.intake().repair_len(), 0, "not queued twice");
    let again = redeliver(batch_one(), world.cursor_value());
    restarted.receive(again, 0, PAST_THE_AGE + 1).unwrap();
    let done = restarted.receive(batch_two(), 0, PAST_THE_AGE + 2).unwrap();
    assert_eq!(done.prefix, 4);
    assert_eq!(restarted.intake().applied_prefix(TENANT), Some(8));
}

#[test]
fn a_stop_before_the_repair_entry_is_stored_loses_nothing_the_hub_cannot_resend() {
    let world = World::new();
    let mut intake = world.start(None).unwrap();
    intake.receive(batch_one(), 0, 1_000).unwrap();
    let mut stopping = world.start(Some(Point::AfterReceive)).unwrap();
    assert!(stopping.receive(Vec::new(), 0, PAST_THE_AGE).is_err());
    // Bundle 2 left parking and the entry was not stored: the cursor still names it as unapplied,
    // so the hub serves it again.
    assert!(world.state.saved.lock().unwrap().repair.is_empty());
    assert!(world.cursor_value() <= 1);
    acknowledged_is_applied(&world, &batch_one());
    let mut restarted = world.start(None).unwrap();
    restarted
        .receive(
            redeliver(batch_one(), world.cursor_value()),
            0,
            PAST_THE_AGE + 1,
        )
        .unwrap();
    let done = restarted.receive(batch_two(), 0, PAST_THE_AGE + 2).unwrap();
    assert_eq!(done.prefix, 4);
}

#[test]
fn a_repair_list_that_cannot_be_stored_gives_the_items_back_and_acknowledges_nothing() {
    let world = World::new();
    let mut intake = world.start(None).unwrap();
    intake.receive(batch_one(), 0, 1_000).unwrap();
    let before = world.cursor_value();
    world.state.saved.lock().unwrap().fail_push = true;
    let error = intake.receive(Vec::new(), 0, PAST_THE_AGE).unwrap_err();
    assert_eq!(error.code, "sync_node_host.store_failed");
    assert_eq!(world.cursor_value(), before, "the cursor did not move");
    assert!(world.state.saved.lock().unwrap().repair.is_empty());

    world.state.saved.lock().unwrap().fail_push = false;
    intake.receive(Vec::new(), 0, PAST_THE_AGE + 1).unwrap();
    assert_eq!(world.state.saved.lock().unwrap().repair.len(), 1);
}

#[test]
fn state_that_cannot_be_read_means_no_intake_at_all() {
    let world = World::new();
    world.state.saved.lock().unwrap().fail_load = true;
    let error = world.start(None).err().expect("restore failed");
    assert_eq!(error.code, "sync_node_host.store_failed");
}

#[test]
fn another_tenants_bundle_is_never_applied_or_counted() {
    let world = World::new();
    let mut intake = world.start(None).unwrap();
    let record = spot_sync_node_host::envelope::encode(
        1,
        "someone-else",
        0,
        &bundle("Customer", "x", 5, &[]),
    )
    .unwrap();
    let foreign = spot_sync_node_host::envelope::decode(&record).unwrap();
    let received = intake.receive(vec![foreign], 0, 1_000).unwrap();
    assert_eq!(
        (received.quarantined, received.applied, received.prefix),
        (1, 0, 0)
    );
    assert!(!world.has("Customer", "x"));
}
