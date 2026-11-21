//! The hub's durable state on a real PostgreSQL: the queue contract shared with the in-memory
//! queue, restart, bounds, tenant isolation, the registry, the relay state and the atomic
//! acknowledgement. Gated on `SPOT_TEST_PG_URL` (skipped with a message when unset, a failure
//! when `SPOT_REQUIRE_PG=1`); the statement inspection needs no server.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{POOL, bridge, fixture, hub, item};
use spot_sync_acks::{NodeRecord, NodeStateStore};
use spot_sync_hub::{AckObserver, DestSnapshot, MemoryQueue, PushOutcome, RelayState, SendQueue};
use spot_sync_hub_pg::{NodeState, PgHub, QueueLimits, STATEMENTS, error};
use spot_sync_master::{Doctype, ScopeKey, Subscription};
use spot_sync_obligations::{Lane, MandatoryEvent};
use spot_sync_protocol::NodeId;

#[test]
fn every_statement_on_a_hub_table_carries_the_tenant() {
    assert!(STATEMENTS.len() > 20);
    for sql in STATEMENTS {
        let reads_or_writes_by_tenant = sql.contains("tenant = $1")
            || (sql.trim_start().starts_with("INSERT") && sql.contains("(tenant,"));
        assert!(reads_or_writes_by_tenant, "no tenant predicate: {sql}");
    }
}

/// The scenario both queues run: the contract in `SendQueue`'s documentation.
fn queue_scenario(q: &mut impl SendQueue) {
    let d = "edge-1";
    // Serve sequences are assigned in push order; a duplicate keeps its number.
    assert_eq!(
        q.push(d, item(Lane::Bulk, 10, 0, false, 100)).unwrap(),
        PushOutcome::Queued
    );
    assert_eq!(
        q.push(d, item(Lane::Bulk, 11, 0, false, 100)).unwrap(),
        PushOutcome::Queued
    );
    assert_eq!(
        q.push(d, item(Lane::Mandatory, 12, 0, false, 50)).unwrap(),
        PushOutcome::Queued
    );
    assert_eq!(
        q.push(d, item(Lane::Bulk, 10, 0, false, 100)).unwrap(),
        PushOutcome::Duplicate
    );
    assert_eq!(
        q.push(d, item(Lane::Bulk, 11, 0, true, 10)).unwrap(),
        PushOutcome::Queued
    );
    assert_eq!(q.last_serve_seq(d).unwrap(), 4);
    assert_eq!(q.queued_bytes(d).unwrap(), 260);
    // Serving order: mandatory first, then position, then index.
    let all = q.peek(d, 10).unwrap();
    let order: Vec<(u64, bool)> = all
        .iter()
        .map(|p| (p.position, p.item.redelivery))
        .collect();
    assert_eq!(
        order,
        vec![(12, false), (10, false), (11, false), (11, true)]
    );
    let seqs: Vec<u64> = all.iter().map(|p| p.item.serve_seq).collect();
    assert_eq!(seqs, vec![3, 1, 2, 4]);
    // serve_after: the greatest queued sequence below each item, over the whole queue.
    let after: Vec<u64> = all.iter().map(|p| p.item.serve_after).collect();
    assert_eq!(after, vec![2, 0, 1, 3]);
    assert_eq!(q.peek(d, 2).unwrap().len(), 2);
    // What would remain after acknowledging serve sequence 2.
    assert_eq!(q.lowest_position_after(d, 2).unwrap(), Some(12));
    assert_eq!(q.lowest_position_after(d, 4).unwrap(), None);
    // Trim by serve sequence removes redeliveries too.
    assert_eq!(q.ack_served(d, 2).unwrap(), 2);
    assert_eq!(q.queued_bytes(d).unwrap(), 60);
    // The counter never falls, and the next push takes the next number.
    assert_eq!(q.last_serve_seq(d).unwrap(), 4);
    assert_eq!(
        q.push(d, item(Lane::Bulk, 13, 0, false, 5)).unwrap(),
        PushOutcome::Queued
    );
    assert_eq!(q.last_serve_seq(d).unwrap(), 5);
    // Position acknowledgement keeps redeliveries; a redelivery leaves only by its own call.
    assert_eq!(q.ack(d, 12).unwrap(), 1);
    assert_eq!(q.remove_redelivered(d, 11).unwrap(), 1);
    assert_eq!(q.remove_redelivered(d, 11).unwrap(), 0);
    // Another destination is independent.
    assert_eq!(q.last_serve_seq("edge-2").unwrap(), 0);
    assert!(q.peek("edge-2", 5).unwrap().is_empty());
}

#[test]
fn the_memory_queue_passes_the_scenario() {
    queue_scenario(&mut MemoryQueue::new());
}

#[test]
fn the_postgres_queue_passes_the_same_scenario() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let hub = hub(&bridge, "t1");
    queue_scenario(&mut hub.queue(QueueLimits::default()));
}

#[test]
fn the_queue_survives_a_restart_and_keeps_its_numbers() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    {
        let hub = hub(&bridge, "t1");
        let mut q = hub.queue(QueueLimits::default());
        for p in 1..=3 {
            q.push("e", item(Lane::Bulk, p, 0, false, 10)).unwrap();
        }
        q.ack_served("e", 2).unwrap();
    }
    // A new store object over the same database.
    let hub = hub(&bridge, "t1");
    let mut q = hub.queue(QueueLimits::default());
    assert_eq!(q.last_serve_seq("e").unwrap(), 3);
    assert_eq!(q.peek("e", 10).unwrap().len(), 1);
    assert_eq!(
        q.push("e", item(Lane::Bulk, 4, 0, false, 10)).unwrap(),
        PushOutcome::Queued
    );
    assert_eq!(q.last_serve_seq("e").unwrap(), 4);
    assert_eq!(
        q.push("e", item(Lane::Bulk, 3, 0, false, 10)).unwrap(),
        PushOutcome::Duplicate
    );
}

#[test]
fn the_queue_bound_is_back_pressure_and_never_a_silent_drop() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let hub = hub(&bridge, "t1");
    let mut q = hub.queue(QueueLimits {
        max_rows: 2,
        max_bytes: 1_000,
    });
    q.push("e", item(Lane::Bulk, 1, 0, false, 10)).unwrap();
    q.push("e", item(Lane::Bulk, 2, 0, false, 10)).unwrap();
    let error = q.push("e", item(Lane::Bulk, 3, 0, false, 10)).unwrap_err();
    assert!(spot_sync_hub_pg::is_queue_full(&error), "{error}");
    assert_eq!(q.peek("e", 10).unwrap().len(), 2, "nothing was dropped");
    assert_eq!(
        q.last_serve_seq("e").unwrap(),
        2,
        "a refused push takes no number"
    );
    // The byte bound.
    let mut q = hub.queue(QueueLimits {
        max_rows: 100,
        max_bytes: 25,
    });
    q.push("b", item(Lane::Bulk, 1, 0, false, 20)).unwrap();
    assert!(spot_sync_hub_pg::is_queue_full(
        &q.push("b", item(Lane::Bulk, 2, 0, false, 10)).unwrap_err()
    ));
    // After a trim there is room again.
    q.ack_served("b", 1).unwrap();
    q.push("b", item(Lane::Bulk, 2, 0, false, 10)).unwrap();
}

#[test]
fn two_tenants_with_the_same_destination_names_never_cross() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let a = hub(&bridge, "tenant_a");
    let b = hub(&bridge, "tenant_b");
    let (mut qa, mut qb) = (
        a.queue(QueueLimits::default()),
        b.queue(QueueLimits::default()),
    );
    qa.push("edge", item(Lane::Bulk, 1, 0, false, 10)).unwrap();
    qa.push("edge", item(Lane::Bulk, 2, 0, false, 10)).unwrap();
    qb.push("edge", item(Lane::Bulk, 1, 0, false, 7)).unwrap();
    assert_eq!(qa.last_serve_seq("edge").unwrap(), 2);
    assert_eq!(qb.last_serve_seq("edge").unwrap(), 1);
    assert_eq!(qb.queued_bytes("edge").unwrap(), 7);
    // B's trim, acknowledgement and counts touch nothing of A.
    assert_eq!(qb.ack_served("edge", 99).unwrap(), 1);
    assert_eq!(qa.peek("edge", 10).unwrap().len(), 2);
    let node = NodeId::new("edge").unwrap();
    let sub = subscription("edge");
    a.nodes()
        .register(&node, "edge", &sub, NodeState::Active, "1", 1)
        .unwrap();
    assert!(
        b.nodes().get(&node).unwrap().is_none(),
        "another tenant's node is missing"
    );
    assert!(b.acks().ack_and_trim("edge", 1, 1, 5).is_err());
    assert_eq!(a.nodes().get(&node).unwrap().unwrap().last_ack_serve_seq, 0);
    // The ledger store refuses another tenant's name outright.
    let mut ledger = a.ledger_store();
    assert!(ledger.load("edge", "tenant_b").is_none());
    assert!(ledger.take_failure());
    ledger.save_event(
        "tenant_b",
        MandatoryEvent {
            position: 1,
            published_at_millis: 0,
            effective_from_millis: None,
            record: "x".to_owned(),
        },
    );
    assert!(b.ledger_store().events("tenant_b").is_empty());
}

fn subscription(node: &str) -> Subscription {
    Subscription::new(
        NodeId::new(node).unwrap(),
        [ScopeKey::new("warehouse", "w1").unwrap()],
        [Doctype::new("item").unwrap()],
    )
}

#[test]
fn the_registry_stores_subscriptions_and_serves_only_active_nodes() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let hub = hub(&bridge, "t1");
    let reg = hub.nodes();
    for (name, state) in [
        ("n-active", NodeState::Active),
        ("n-suspended", NodeState::Suspended),
        ("n-enrolled", NodeState::Enrolled),
    ] {
        let node = NodeId::new(name).unwrap();
        reg.register(&node, "edge", &subscription(name), state, "1.0-1.2", 100)
            .unwrap();
    }
    let dup = reg.register(
        &NodeId::new("n-active").unwrap(),
        "edge",
        &subscription("n-active"),
        NodeState::Active,
        "",
        101,
    );
    assert_eq!(dup.unwrap_err().code(), error::NODE_REFUSED);
    assert_eq!(reg.active_nodes().unwrap(), vec!["n-active".to_owned()]);
    let active = reg.active_router().unwrap();
    assert_eq!(active.router.len(), 1);
    assert!(active.invalid.is_empty());
    // A stored subscription round-trips through its validated form.
    let node = NodeId::new("n-active").unwrap();
    assert_eq!(
        reg.subscription(&node).unwrap(),
        Some(subscription("n-active"))
    );
    // Revocation is immediate and final.
    reg.set_state(&node, NodeState::Revoked, 200).unwrap();
    assert!(reg.active_nodes().unwrap().is_empty());
    assert_eq!(
        reg.set_state(&node, NodeState::Active, 201)
            .unwrap_err()
            .code(),
        error::NODE_REFUSED
    );
    assert_eq!(reg.get(&node).unwrap().unwrap().state, NodeState::Revoked);
}

#[test]
fn an_invalid_stored_subscription_makes_the_node_receive_nothing() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let hub = hub(&bridge, "t1");
    let reg = hub.nodes();
    let node = NodeId::new("n1").unwrap();
    reg.register(
        &node,
        "edge",
        &subscription("n1"),
        NodeState::Active,
        "1",
        1,
    )
    .unwrap();
    let good = NodeId::new("n2").unwrap();
    reg.register(
        &good,
        "edge",
        &subscription("n2"),
        NodeState::Active,
        "1",
        1,
    )
    .unwrap();
    // Damage the stored form out of band (as an operator mistake or a bad migration might).
    for damage in [
        "not json",
        r#"{"node":"n1","scope_keys":[],"doctypes":[],"extra":1}"#,
        r#"{"node":"other","scope_keys":[],"doctypes":["item"]}"#,
        r#"{"node":"n1","scope_keys":[{"key_type":"","value":""}],"doctypes":[]}"#,
    ] {
        let client = fx.rt.block_on(fx.db.raw_client());
        fx.rt
            .block_on(client.execute(
                "UPDATE spot_sync_node SET subscription_json = $1 WHERE tenant = 't1' AND node_id = 'n1'",
                &[&damage],
            ))
            .unwrap();
        assert_eq!(
            reg.subscription(&node).unwrap_err().code(),
            error::SUBSCRIPTION_INVALID,
            "{damage}"
        );
        let active = reg.active_router().unwrap();
        assert_eq!(active.invalid, vec!["n1".to_owned()], "{damage}");
        assert_eq!(active.router.len(), 1, "never widened to everything");
        assert!(reg.get(&node).unwrap().unwrap().subscription.is_none());
    }
}

fn state_with(cursor: u64) -> RelayState {
    let mut destinations = BTreeMap::new();
    destinations.insert(
        "edge-1".to_owned(),
        DestSnapshot {
            holds: Default::default(),
            held_entries: BTreeMap::new(),
            held_by_dependency: BTreeMap::new(),
            release_backlog: BTreeMap::new(),
            extras: Default::default(),
            acked: Some(cursor),
            catchup_from: None,
        },
    );
    RelayState {
        cursor,
        last_txn: Some("txn-9".to_owned()),
        lane_of_entity: BTreeMap::from([("item".to_owned(), Lane::Bulk)]),
        destinations,
    }
}

#[test]
fn the_relay_state_round_trips_and_a_restart_sees_it() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    {
        let hub = hub(&bridge, "t1");
        assert_eq!(hub.relay_state().load().unwrap(), None);
        hub.relay_state().save(&state_with(40)).unwrap();
        hub.relay_state().save(&state_with(42)).unwrap();
    }
    let hub = hub(&bridge, "t1");
    assert_eq!(hub.relay_state().load().unwrap(), Some(state_with(42)));
    // Another tenant has none.
    assert_eq!(
        common::hub(&bridge, "t2").relay_state().load().unwrap(),
        None
    );
    // The queue's numbers are not touched by a state save.
    let mut q = hub.queue(QueueLimits::default());
    q.push("edge-1", item(Lane::Bulk, 1, 0, false, 1)).unwrap();
    hub.relay_state().save(&state_with(43)).unwrap();
    assert_eq!(q.last_serve_seq("edge-1").unwrap(), 1);
    // A destination dropped from the state loses its snapshot only.
    let mut smaller = state_with(44);
    smaller.destinations.clear();
    hub.relay_state().save(&smaller).unwrap();
    assert_eq!(hub.relay_state().load().unwrap(), Some(smaller));
    assert_eq!(q.peek("edge-1", 5).unwrap().len(), 1);
}

#[test]
fn a_damaged_snapshot_is_refused_whole() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let hub = hub(&bridge, "t1");
    hub.relay_state().save(&state_with(5)).unwrap();
    let client = fx.rt.block_on(fx.db.raw_client());
    fx.rt
        .block_on(client.execute(
            "UPDATE spot_sync_hub_dest_state SET snapshot = '{\"holds\":1}' WHERE tenant = 't1'",
            &[],
        ))
        .unwrap();
    assert_eq!(
        hub.relay_state().load().unwrap_err().code(),
        error::STATE_INVALID
    );
}

fn register_active(hub: &PgHub, name: &str) {
    let node = NodeId::new(name).unwrap();
    hub.nodes()
        .register(
            &node,
            "edge",
            &subscription(name),
            NodeState::Active,
            "1",
            1,
        )
        .unwrap();
}

#[test]
fn an_acknowledgement_trims_and_records_in_one_step() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let hub = hub(&bridge, "t1");
    register_active(&hub, "edge");
    let mut q = hub.queue(QueueLimits::default());
    for p in 1..=5 {
        q.push("edge", item(Lane::Bulk, p, 0, false, 10)).unwrap();
    }
    let acks = hub.acks();
    let done = acks.ack_and_trim("edge", 3, 3, 1_000).unwrap();
    assert_eq!((done.removed, done.last_ack_serve_seq), (3, 3));
    assert_eq!(q.peek("edge", 10).unwrap().len(), 2);
    let row = hub
        .nodes()
        .get(&NodeId::new("edge").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.last_ack_serve_seq, row.applied_prefix, row.last_seen),
        (3, 3, Some(1_000))
    );
    // Nothing moves backwards.
    let again = acks.ack_and_trim("edge", 1, 1, 500).unwrap();
    assert_eq!((again.removed, again.last_ack_serve_seq), (0, 3));
    let row = hub
        .nodes()
        .get(&NodeId::new("edge").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!((row.applied_prefix, row.last_seen), (3, Some(1_000)));
}

/// The crash-safety rule: a refused acknowledgement leaves both the queue and the node row as
/// they were (the failure is forced by naming a serve sequence the queue never issued, and an
/// unknown node, each of which fails inside the transaction after or before the delete).
#[test]
fn a_refused_acknowledgement_changes_neither_the_queue_nor_the_node() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let hub = hub(&bridge, "t1");
    register_active(&hub, "edge");
    let mut q = hub.queue(QueueLimits::default());
    for p in 1..=3 {
        q.push("edge", item(Lane::Bulk, p, 0, false, 10)).unwrap();
    }
    // Beyond what was issued.
    assert_eq!(
        hub.acks()
            .ack_and_trim("edge", 99, 3, 1)
            .unwrap_err()
            .code(),
        error::NODE_REFUSED
    );
    assert_eq!(q.peek("edge", 10).unwrap().len(), 3);
    // An unknown destination has no node: the trim must not happen without the record.
    q.push("ghost", item(Lane::Bulk, 1, 0, false, 10)).unwrap();
    assert_eq!(
        hub.acks()
            .ack_and_trim("ghost", 1, 1, 1)
            .unwrap_err()
            .code(),
        error::NODE_REFUSED
    );
    assert_eq!(
        q.peek("ghost", 10).unwrap().len(),
        1,
        "the queue was not trimmed past an unrecorded ack"
    );
    // A revoked node cannot acknowledge.
    hub.nodes()
        .set_state(&NodeId::new("edge").unwrap(), NodeState::Revoked, 2)
        .unwrap();
    assert!(hub.acks().ack_and_trim("edge", 1, 1, 3).is_err());
    assert_eq!(q.peek("edge", 10).unwrap().len(), 3);
}

#[test]
fn the_observer_records_a_prefix_forward_only() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let hub = hub(&bridge, "t1");
    register_active(&hub, "edge");
    let mut observer = hub.acks();
    observer.on_ack("edge", 7);
    observer.on_ack("edge", 5);
    assert!(!observer.take_failure());
    let row = hub
        .nodes()
        .get(&NodeId::new("edge").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(row.applied_prefix, 7);
}

#[test]
fn the_ledger_store_round_trips_records_and_events() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let hub = hub(&bridge, "t1");
    let mut store = hub.ledger_store();
    assert!(store.load("edge", "t1").is_none());
    let record = NodeRecord {
        node: "edge".to_owned(),
        tenant: "t1".to_owned(),
        applied_prefix: 9,
        holds: vec![4],
        last_contact_millis: 77,
        version: "1.2".to_owned(),
        level: spot_sync_obligations::Level::Ok,
        crossings: Vec::new(),
    };
    store.save(record.clone());
    store.save_event(
        "t1",
        MandatoryEvent {
            position: 3,
            published_at_millis: 10,
            effective_from_millis: Some(20),
            record: "tax_rate/v7".to_owned(),
        },
    );
    assert!(!store.take_failure());
    // A new store object (a restart) sees both.
    let fresh = hub.ledger_store();
    assert_eq!(fresh.load("edge", "t1"), Some(record));
    assert_eq!(fresh.nodes(), vec![("edge".to_owned(), "t1".to_owned())]);
    assert_eq!(fresh.events("t1").len(), 1);
}

#[test]
fn a_hundred_destinations_with_unequal_progress() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 256);
    let hub = hub(&bridge, "t1");
    let mut q = hub.queue(QueueLimits::default());
    for d in 0..100u64 {
        let dest = format!("dest-{d}");
        for p in 1..=(d % 7 + 1) {
            q.push(&dest, item(Lane::Bulk, p, 0, false, 10)).unwrap();
        }
        q.ack_served(&dest, d % 3).unwrap();
    }
    for d in 0..100u64 {
        let dest = format!("dest-{d}");
        let pushed = d % 7 + 1;
        let acked = (d % 3).min(pushed);
        assert_eq!(
            q.peek(&dest, 20).unwrap().len() as u64,
            pushed - acked,
            "{dest}"
        );
        assert_eq!(q.last_serve_seq(&dest).unwrap(), pushed, "{dest}");
    }
}

/// The trim budget: 10,000 acknowledged rows in a 200,000-row queue under 200 ms. The numbers are
/// printed for the report; the assertion is the brief's budget.
#[test]
fn trimming_ten_thousand_rows_of_a_large_queue_meets_its_budget() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let hub = hub(&bridge, "t1");
    let client = fx.rt.block_on(fx.db.raw_client());
    // Bulk-load directly: 200,000 pushes through the trait would measure the bridge, not the trim.
    fx.rt
        .block_on(client.batch_execute(
            "INSERT INTO spot_sync_hub_dest_state (tenant, destination, last_serve_seq) \
             VALUES ('t1', 'big', 200000); \
             INSERT INTO spot_sync_hub_queue (tenant, destination, serve_seq, position, bundle_index, \
                 redelivery, lane, bytes, entry) \
             SELECT 't1', 'big', g, g, 0, false, 1, 10, \
                 '{\"lane\":\"Bulk\",\"position\":0,\"item\":{\"index\":0,\"redelivery\":false,\
                 \"bundle\":{\"kind\":\"Root\",\"records\":[],\"bytes\":10}}}' \
             FROM generate_series(1, 200000) g;",
        ))
        .unwrap();
    let mut q = hub.queue(QueueLimits {
        max_rows: 10_000_000,
        max_bytes: u64::MAX / 2,
    });
    let started = std::time::Instant::now();
    let removed = q.ack_served("big", 10_000).unwrap();
    let took = started.elapsed();
    println!("TRIM 10,000 of 200,000 rows: {took:?}");
    assert_eq!(removed, 10_000);
    assert!(took < std::time::Duration::from_millis(200), "{took:?}");
    let _ = (POOL, Arc::strong_count(&bridge));
}

#[test]
fn building_a_store_inside_an_async_runtime_is_refused() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let result = fx
        .rt
        .block_on(async { PgHub::new(Arc::clone(&bridge), POOL, "t1").map(|_| ()) });
    assert_eq!(result.unwrap_err().code(), error::UNSUPPORTED_CONTEXT);
    // And an unusable tenant on a plain thread.
    assert!(PgHub::new(Arc::clone(&bridge), POOL, "").is_err());
}

#[test]
fn missing_tables_are_reported_and_the_host_can_refuse_to_serve() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(&fx, 4, 64);
    let raw = PgHub::new(Arc::clone(&bridge), POOL, "t1").unwrap();
    assert_eq!(
        raw.db().missing_tables().unwrap().len(),
        spot_sync_hub_pg::schema::TABLES.len()
    );
    assert_eq!(
        raw.db().require_tables().unwrap_err().code(),
        error::TABLES_MISSING
    );
    raw.prepare().unwrap();
    raw.prepare().unwrap();
    assert!(raw.db().missing_tables().unwrap().is_empty());
    raw.db().require_tables().unwrap();
}
