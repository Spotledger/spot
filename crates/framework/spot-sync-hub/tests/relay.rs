//! Routing, closure, holds, restart, isolation, lanes and tenancy of the hub relay.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use serde_json::json;
use spot_sync_closure::{RecordKey, Role};
use spot_sync_hub::{
    Destination, DestinationScope, HubRelay, MemoryStream, RelayConfig, SendQueue, StreamEntry,
};
use spot_sync_obligations::Lane;

type Relay = HubRelay<SharedQueue, MemoryStream, Src>;

fn relay_with(cfg: RelayConfig, stream: MemoryStream, src: Src, queue: SharedQueue) -> Relay {
    HubRelay::new(cfg, stream, queue, src, Box::new(Refs)).unwrap()
}

fn stream_of(entries: Vec<StreamEntry>) -> MemoryStream {
    let mut s = MemoryStream::new();
    for e in entries {
        s.append(e);
    }
    s
}

fn keys(queue: &SharedQueue, dest: &str) -> Vec<RecordKey> {
    served(queue, dest).into_iter().map(|(k, _)| k).collect()
}

#[test]
fn two_destinations_with_different_scopes_see_different_streams() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 1);
    src.put("price", "P1", plain(), 2);
    src.put("item", "I2", plain(), 3);
    let stream = stream_of(vec![
        entry(1, "a", "item", "I1", 1),
        entry(2, "b", "price", "P1", 1),
        entry(3, "c", "item", "I2", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("items", &["item"])).unwrap();
    r.add_destination(dest("prices", &["price"])).unwrap();
    r.add_destination(dest("nothing", &["tax"])).unwrap();
    let rep = r.tick(10).unwrap();
    assert_eq!(rep.cursor, 3);
    assert_eq!(keys(&q, "items"), vec![k("item", "I1"), k("item", "I2")]);
    assert_eq!(keys(&q, "prices"), vec![k("price", "P1")]);
    assert!(keys(&q, "nothing").is_empty());
}

#[test]
fn closure_pulls_a_referenced_record_outside_scope_and_remembers_it_as_an_extra() {
    let mut src = Src::default();
    src.put("master", "M1", plain(), 1);
    src.put("order", "O1", linking(&["M1"]), 2);
    src.put("master", "M1", json!({ "note": "v2", "secret": "s" }), 4);
    src.put("order", "O2", linking(&[]), 3);
    let stream = stream_of(vec![
        entry(2, "a", "order", "O1", 1),
        entry(3, "b", "order", "O2", 1),
        entry(4, "c", "master", "M1", 2),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("d", &["order"])).unwrap();
    r.tick(0).unwrap();
    let all = q.0.borrow().all("d");
    // The order bundle carries master M1 as an extra, first.
    let first = &all[0].item.bundle.records;
    assert_eq!(first[0].key, k("master", "M1"));
    assert_eq!(first[0].role, Role::Extra);
    assert_eq!(first[1].key, k("order", "O1"));
    // The extra is recorded, so the later version of M1 (out of scope) is routed.
    assert!(r.extras("d").unwrap().contains(&k("master", "M1")));
    assert!(
        keys(&q, "d")
            .iter()
            .filter(|x| **x == k("master", "M1"))
            .count()
            >= 2
    );
}

#[test]
fn nothing_unredacted_is_queued() {
    let mut src = Src::default();
    src.put("master", "M1", plain(), 1);
    src.put("order", "O1", linking(&["M1"]), 2);
    let stream = stream_of(vec![entry(2, "a", "order", "O1", 1)]);
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("d", &["order"])).unwrap();
    r.tick(0).unwrap();
    let all = q.0.borrow().all("d");
    assert!(!all.is_empty());
    for p in &all {
        for rec in &p.item.bundle.records {
            assert!(rec.image.get("secret").is_none(), "{:?}", rec.key);
        }
    }
}

#[test]
fn records_of_another_tenant_or_source_are_dropped_for_every_destination() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 1);
    src.put("item", "X1", plain(), 2);
    src.put("item", "X2", plain(), 3);
    let mut other_tenant = entry(2, "b", "item", "X1", 1);
    other_tenant.tenant = "t2".to_owned();
    let mut other_source = entry(3, "c", "item", "X2", 1);
    other_source.source = "elsewhere".to_owned();
    let stream = stream_of(vec![
        entry(1, "a", "item", "I1", 1),
        other_tenant,
        other_source,
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("a", &["item"])).unwrap();
    r.add_destination(dest("b", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.foreign_dropped, 2);
    for d in ["a", "b"] {
        assert_eq!(keys(&q, d), vec![k("item", "I1")], "{d}");
    }
}

#[test]
fn an_entry_is_never_sent_back_to_its_origin() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 1);
    let mut e = entry(1, "a", "item", "I1", 1);
    e.origin = Some("branch1".to_owned());
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream_of(vec![e]), src, q.clone());
    r.add_destination(dest("branch1", &["item"])).unwrap();
    r.add_destination(dest("branch2", &["item"])).unwrap();
    r.tick(0).unwrap();
    assert!(keys(&q, "branch1").is_empty());
    assert_eq!(keys(&q, "branch2").len(), 1);
}

#[test]
fn a_held_record_is_withheld_then_released_in_order() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 3);
    src.put("item", "I2", plain(), 4);
    let mut e2 = entry(2, "b", "item", "I1", 2);
    e2.major = 2;
    let stream = stream_of(vec![
        entry(1, "a", "item", "I1", 1),
        e2,
        entry(3, "c", "item", "I1", 3),
        entry(4, "d", "item", "I2", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("d", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.destinations["d"].held, 2);
    // Position 1 and the unrelated record go; versions 2 and 3 of I1 wait.
    let positions: Vec<u64> = served(&q, "d").iter().map(|(_, p)| *p).collect();
    assert_eq!(positions, vec![3, 4]);
    assert_eq!(r.held_positions("d"), vec![2, 3]);
    // Upgrade: both come back, oldest first, as redeliveries.
    let rel = r
        .on_node_upgraded("d", report(&[("item", &[1, 2])]))
        .unwrap();
    assert_eq!(rel.released, 2);
    assert_eq!(rel.bundles_queued, 2);
    assert!(rel.missing.is_empty());
    let all = q.0.borrow().all("d");
    let redelivered: Vec<u64> = all
        .iter()
        .filter(|p| p.item.redelivery)
        .map(|p| p.position)
        .collect();
    assert_eq!(redelivered, vec![2, 3]);
    // The node's prefix passed the held positions, yet an ack does not trim the redeliveries.
    r.on_ack("d", 4).unwrap();
    let left = q.0.borrow().all("d");
    assert_eq!(left.len(), 2);
    assert!(left.iter().all(|p| p.item.redelivery));
    r.on_redelivery_ack("d", &[2, 3]).unwrap();
    assert!(q.0.borrow().all("d").is_empty());
    assert!(r.held_positions("d").is_empty());
}

#[test]
fn restart_replay_produces_no_duplicate_queue_entries() {
    let mut src = Src::default();
    src.put("master", "M1", plain(), 1);
    src.put("item", "I1", linking(&["M1"]), 2);
    src.put("item", "I2", plain(), 3);
    let entries = vec![
        entry(2, "a", "item", "I1", 1),
        entry(3, "b", "item", "I2", 1),
    ];
    let q = SharedQueue::default();
    let mut r1 = relay_with(cfg(), stream_of(entries.clone()), src.clone(), q.clone());
    r1.add_destination(dest("d", &["item"])).unwrap();
    r1.tick(0).unwrap();
    let before = q.0.borrow().all("d");
    // A restart that lost the cursor: a new relay reads everything again.
    let mut r2 = relay_with(cfg(), stream_of(entries), src, q.clone());
    r2.add_destination(dest("d", &["item"])).unwrap();
    let rep = r2.tick(1).unwrap();
    assert_eq!(q.0.borrow().all("d"), before);
    assert_eq!(rep.destinations["d"].bundles_queued, 0);
    assert_eq!(rep.destinations["d"].bundles_duplicate, before.len());
}

#[test]
fn snapshot_and_restore_carry_the_cursor_and_the_holds() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 1);
    let mut e = entry(1, "a", "item", "I1", 1);
    e.major = 2;
    let q = SharedQueue::default();
    let mut r1 = relay_with(cfg(), stream_of(vec![e.clone()]), src.clone(), q.clone());
    r1.add_destination(dest("d", &["item"])).unwrap();
    r1.tick(0).unwrap();
    let snap = r1.snapshot();
    // The snapshot is plain serde data. (`spot-sync-skew`'s `HoldSet` has tuple map keys, which
    // JSON cannot carry; a binary format can. See the task report.)
    let mut r2 = relay_with(cfg(), stream_of(vec![e]), src, q.clone());
    r2.add_destination(dest("d", &["item"])).unwrap();
    r2.restore(snap.clone()).unwrap();
    assert_eq!(r2.cursor(), 1);
    assert_eq!(r2.held_positions("d"), vec![1]);
    let rel = r2
        .on_node_upgraded("d", report(&[("item", &[1, 2])]))
        .unwrap();
    assert_eq!(rel.bundles_queued, 1);
}

#[test]
fn a_full_queue_parks_one_destination_and_the_others_carry_on() {
    let mut src = Src::default();
    let mut entries = Vec::new();
    for i in 1..=20u64 {
        let id = format!("I{i}");
        src.put("item", &id, plain(), i);
        entries.push(entry(i, &format!("t{i}"), "item", &id, 1));
    }
    let mut c = cfg();
    c.read_batch = 5;
    c.max_queue_bytes = 150;
    let q = SharedQueue::default();
    let mut r = relay_with(c, stream_of(entries), src, q.clone());
    for d in ["fast1", "slow", "fast2"] {
        r.add_destination(dest(d, &["item"])).unwrap();
    }
    let mut saw_backpressure = false;
    for tick in 0..4 {
        let rep = r.tick(tick).unwrap();
        saw_backpressure |= rep.destinations["slow"].backpressured;
        assert!(!rep.destinations["fast1"].backpressured);
        assert!(!rep.destinations["fast2"].backpressured);
        let cursor = r.cursor();
        r.on_ack("fast1", cursor).unwrap();
        r.on_ack("fast2", cursor).unwrap();
    }
    assert!(saw_backpressure);
    assert!(r.behind_from("slow").is_some());
    assert!(r.behind_from("fast1").is_none());
    // The fast ones were served everything (their queues were trimmed by acks, so count via the
    // slow one's gap): the slow queue holds fewer than 20 records.
    assert!(served(&q, "slow").len() < 20);
    // The slow node drains; the relay catches it up, in position order, without gaps.
    for tick in 4..40 {
        let applied = served(&q, "slow").last().map_or(0, |(_, p)| *p);
        if applied > 0 {
            r.on_ack("slow", applied).unwrap();
        }
        r.tick(tick).unwrap();
        if r.behind_from("slow").is_none() {
            break;
        }
    }
    assert!(r.behind_from("slow").is_none());
}

#[test]
fn the_slow_destination_receives_every_record_exactly_once_in_order() {
    let mut src = Src::default();
    let mut entries = Vec::new();
    for i in 1..=12u64 {
        let id = format!("I{i}");
        src.put("item", &id, plain(), i);
        entries.push(entry(i, &format!("t{i}"), "item", &id, 1));
    }
    let mut c = cfg();
    c.read_batch = 4;
    c.max_queue_bytes = 120;
    let q = SharedQueue::default();
    let mut r = relay_with(c, stream_of(entries), src, q.clone());
    r.add_destination(dest("slow", &["item"])).unwrap();
    let mut got: Vec<u64> = Vec::new();
    for tick in 0..60 {
        r.tick(tick).unwrap();
        // The transport sends and the node acknowledges whatever is queued.
        let batch = q.0.borrow().all("slow");
        for p in &batch {
            got.push(p.position);
        }
        if let Some(last) = batch.last() {
            r.on_ack("slow", last.position).unwrap();
        }
        if r.cursor() >= 12 && r.behind_from("slow").is_none() && batch.is_empty() {
            break;
        }
    }
    assert_eq!(got, (1..=12).collect::<Vec<u64>>());
}

#[test]
fn a_failing_queue_for_one_destination_does_not_stop_the_others() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 1);
    src.put("item", "I2", plain(), 2);
    let stream = stream_of(vec![
        entry(1, "a", "item", "I1", 1),
        entry(2, "b", "item", "I2", 1),
    ]);
    let q = SharedQueue::default();
    q.0.borrow_mut().set_failing("bad", true);
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("bad", &["item"])).unwrap();
    r.add_destination(dest("good", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(
        rep.destinations["bad"].failed,
        Some("sync_hub.queue_failed")
    );
    assert!(rep.destinations["bad"].behind);
    assert_eq!(keys(&q, "good").len(), 2);
    q.0.borrow_mut().set_failing("bad", false);
    r.tick(1).unwrap();
    assert_eq!(keys(&q, "bad").len(), 2);
    assert!(r.behind_from("bad").is_none());
}

#[test]
fn a_missing_image_fails_that_destination_loudly_and_keeps_the_cursor_moving() {
    let mut src = Src::default();
    src.put("item", "I2", plain(), 2);
    let stream = stream_of(vec![
        entry(1, "a", "item", "GONE", 1),
        entry(2, "b", "item", "I2", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("d", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.destinations["d"].failed, Some("sync_hub.image_missing"));
    assert_eq!(rep.cursor, 2);
    assert_eq!(r.behind_from("d"), Some(1));
}

#[test]
fn at_most_the_per_tick_cap_of_bundles_goes_to_one_destination() {
    let mut src = Src::default();
    let mut entries = Vec::new();
    for i in 1..=10u64 {
        let id = format!("I{i}");
        src.put("item", &id, plain(), i);
        entries.push(entry(i, &format!("t{i}"), "item", &id, 1));
    }
    let mut c = cfg();
    c.max_bundles_per_tick_per_dest = 3;
    let q = SharedQueue::default();
    let mut r = relay_with(c, stream_of(entries), src, q.clone());
    r.add_destination(dest("returning", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.destinations["returning"].bundles_queued, 3);
    assert_eq!(r.behind_from("returning"), Some(4));
    r.tick(1).unwrap();
    assert_eq!(q.0.borrow().all("returning").len(), 6);
    for t in 2..10 {
        r.tick(t).unwrap();
    }
    let got: Vec<u64> =
        q.0.borrow()
            .all("returning")
            .iter()
            .map(|p| p.position)
            .collect();
    assert_eq!(got, (1..=10).collect::<Vec<u64>>());
}

#[test]
fn a_mandatory_record_behind_many_ordinary_ones_is_served_first() {
    let mut src = Src::default();
    let mut entries = Vec::new();
    for i in 1..=30u64 {
        let id = format!("I{i}");
        src.put("item", &id, plain(), i);
        entries.push(entry(i, &format!("t{i}"), "item", &id, 1));
    }
    src.put("tax", "T1", plain(), 31);
    let mut tax = entry(31, "tax", "tax", "T1", 1);
    tax.lane = Lane::Mandatory;
    entries.push(tax);
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream_of(entries), src, q.clone());
    r.add_destination(dest("d", &["item", "tax"])).unwrap();
    r.tick(0).unwrap();
    let first = q.0.borrow().peek("d", 1).unwrap();
    assert_eq!(first[0].lane, Lane::Mandatory);
    assert_eq!(first[0].item.bundle.records[0].key, k("tax", "T1"));
}

#[test]
fn an_ack_that_regresses_or_runs_ahead_is_refused_and_changes_nothing() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 1);
    src.put("item", "I2", plain(), 2);
    let stream = stream_of(vec![
        entry(1, "a", "item", "I1", 1),
        entry(2, "b", "item", "I2", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("d", &["item"])).unwrap();
    r.tick(0).unwrap();
    assert_eq!(r.on_ack("d", 1).unwrap(), 1);
    let err = r.on_ack("d", 0).unwrap_err();
    assert_eq!(err.code(), "sync_hub.prefix_regressed");
    let err = r.on_ack("d", 99).unwrap_err();
    assert_eq!(err.code(), "sync_hub.prefix_ahead_of_stream");
    assert_eq!(q.0.borrow().all("d").len(), 1);
    assert_eq!(
        r.on_ack("nobody", 1).unwrap_err().code(),
        "sync_hub.unknown_destination"
    );
}

#[test]
fn a_full_hold_set_reports_unstored_entries_without_queueing_them() {
    let mut src = Src::default();
    for i in 1..=3u64 {
        src.put("item", &format!("I{i}"), plain(), i);
    }
    let entries: Vec<StreamEntry> = (1..=3u64)
        .map(|i| {
            let mut e = entry(i, &format!("t{i}"), "item", &format!("I{i}"), 1);
            e.major = 2;
            e
        })
        .collect();
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream_of(entries), src, q.clone());
    let mut d = dest("d", &["item"]);
    d.hold_limit = 1;
    r.add_destination(d).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.destinations["d"].held, 1);
    assert_eq!(rep.destinations["d"].unstored, 2);
    assert!(q.0.borrow().all("d").is_empty());
}

#[test]
fn registration_and_configuration_are_validated() {
    let q = SharedQueue::default();
    let mut bad = cfg();
    bad.tenant.clear();
    let err = HubRelay::new(
        bad,
        MemoryStream::new(),
        q.clone(),
        Src::default(),
        Box::new(Refs),
    )
    .err()
    .unwrap();
    assert_eq!(err.code(), "sync_hub.invalid_config");
    let mut zero = cfg();
    zero.max_queue_bytes = 0;
    assert!(zero.validate().is_err());
    let mut r = relay_with(cfg(), MemoryStream::new(), Src::default(), q);
    r.add_destination(dest("d", &["item"])).unwrap();
    let err = r.add_destination(dest("d", &["item"])).unwrap_err();
    assert_eq!(err.code(), "sync_hub.duplicate_destination");
}

#[test]
fn a_stream_that_runs_backwards_is_refused_and_nothing_moves() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 1);
    let stream = stream_of(vec![
        entry(2, "a", "item", "I1", 1),
        entry(1, "b", "item", "I1", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("d", &["item"])).unwrap();
    assert_eq!(r.tick(0).unwrap_err().code(), "sync_hub.stream_disorder");
    assert_eq!(r.cursor(), 0);
    assert!(q.0.borrow().all("d").is_empty());
}

#[test]
fn every_code_is_in_the_catalogue() {
    let toml = include_str!("../../spot-errors/catalogue/sync_hub.toml");
    for code in [
        spot_sync_hub::INVALID_CONFIG,
        spot_sync_hub::UNKNOWN_DESTINATION,
        spot_sync_hub::DUPLICATE_DESTINATION,
        spot_sync_hub::PREFIX_REGRESSED,
        spot_sync_hub::PREFIX_AHEAD,
        spot_sync_hub::STREAM_FAILED,
        spot_sync_hub::STREAM_DISORDER,
        spot_sync_hub::QUEUE_FAILED,
        spot_sync_hub::IMAGE_MISSING,
        spot_sync_hub::HELD_BY_DEPENDENCY,
    ] {
        assert!(toml.contains(&format!("code = \"{code}\"")), "{code}");
    }
}

/// A scope whose entitlement can be revoked while the relay holds the destination's entries.
struct Revocable(std::rc::Rc<std::cell::Cell<bool>>);

impl DestinationScope for Revocable {
    fn covers(&self, entry: &StreamEntry) -> bool {
        self.0.get() && entry.key.entity == "item"
    }
    fn covers_key(&self, key: &RecordKey) -> bool {
        self.0.get() && key.entity == "item"
    }
    fn owns(&self, _key: &RecordKey) -> bool {
        false
    }
}

#[test]
fn a_held_record_is_not_released_after_the_scope_was_revoked() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 2);
    let mut e1 = entry(1, "a", "item", "I1", 2);
    e1.major = 2;
    let q = SharedQueue::default();
    let mut r = relay_with(cfg(), stream_of(vec![e1]), src, q.clone());
    let live = std::rc::Rc::new(std::cell::Cell::new(true));
    r.add_destination(Destination::new(
        "d",
        Box::new(Revocable(live.clone())),
        Box::new(StripSecret),
        v1(&["item"]),
    ))
    .unwrap();
    r.tick(0).unwrap();
    assert_eq!(r.held_positions("d"), vec![1]);
    live.set(false);
    let rel = r
        .on_node_upgraded("d", report(&[("item", &[1, 2])]))
        .unwrap();
    assert_eq!(rel.bundles_queued, 0);
    assert_eq!(rel.pending, 0);
    assert!(q.0.borrow().all("d").is_empty());
}
