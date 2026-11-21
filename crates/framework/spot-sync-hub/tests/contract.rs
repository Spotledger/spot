//! The relay verifies the `HubStream` contract instead of trusting it: a read that splits a
//! source transaction, interleaves two, or changes an entity's lane is refused whole, with
//! nothing queued and the cursor where it was.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use spot_sync_hub::{HubError, HubRelay, HubStream, RelayConfig, StreamEntry};
use spot_sync_obligations::Lane;

type Relay<S> = HubRelay<SharedQueue, S, Src>;

fn relay_with<S: HubStream>(cfg: RelayConfig, stream: S, src: Src, q: SharedQueue) -> Relay<S> {
    HubRelay::new(cfg, stream, q, src, Box::new(Refs)).unwrap()
}

fn items(src: &mut Src, n: u64) {
    for i in 1..=n {
        src.put("item", &format!("I{i}"), plain(), i);
    }
}

/// One transaction of `len` item entries at the positions that follow `first`.
fn txn(first: u64, txn: &str, len: u64) -> Vec<StreamEntry> {
    (0..len)
        .map(|i| entry(first + i, txn, "item", &format!("I{}", first + i), 1))
        .collect()
}

#[test]
fn a_read_that_cuts_a_transaction_is_refused_and_nothing_more_is_queued() {
    let mut src = Src::default();
    items(&mut src, 4);
    let mut entries = txn(1, "a", 3);
    entries.push(entry(4, "b", "item", "I4", 1));
    let mut c = cfg();
    c.read_batch = 2;
    let q = SharedQueue::default();
    let mut r = relay_with(c, SplittingStream::of(entries), src, q.clone());
    r.add_destination(dest("d", &["item"])).unwrap();
    // The first read stopped inside transaction a, which nothing could know yet: what it returned
    // was routed.
    let first = r.tick(0).unwrap();
    assert_eq!(first.failed, None);
    assert_eq!(r.cursor(), 2);
    let queued = served(&q, "d");
    assert_eq!(queued.len(), 2);
    // The next read would continue transaction a: refused, nothing queued, the cursor stays.
    let second = r.tick(1).unwrap();
    assert_eq!(second.failed, Some("sync_hub.stream_split_txn"));
    assert_eq!(second.cursor, 2);
    assert_eq!(r.cursor(), 2);
    assert_eq!(served(&q, "d"), queued);
    assert_eq!(
        second.destinations["d"].failed,
        Some("sync_hub.stream_split_txn")
    );
    // A permanent adapter fault stays visible: the next tick asks again and gets the same answer.
    let third = r.tick(2).unwrap();
    assert_eq!(third.failed, Some("sync_hub.stream_split_txn"));
    assert_eq!(r.cursor(), 2);
    assert_eq!(served(&q, "d"), queued);
}

#[test]
fn an_interleaved_transaction_is_refused_before_anything_is_queued() {
    let mut src = Src::default();
    items(&mut src, 4);
    let q = SharedQueue::default();
    let stream = InterleavingStream::of(vec![
        vec![
            entry(0, "a", "item", "I1", 1),
            entry(0, "a", "item", "I2", 1),
        ],
        vec![
            entry(0, "b", "item", "I3", 1),
            entry(0, "b", "item", "I4", 1),
        ],
    ]);
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("d", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.failed, Some("sync_hub.stream_split_txn"));
    assert_eq!(rep.cursor, 0);
    assert_eq!(r.cursor(), 0);
    assert!(served(&q, "d").is_empty());
    assert_eq!(
        rep.destinations["d"].failed,
        Some("sync_hub.stream_split_txn")
    );
}

#[test]
fn a_lane_that_changes_is_refused_and_nothing_is_queued() {
    let mut src = Src::default();
    items(&mut src, 2);
    let entries = vec![
        entry(1, "a", "item", "I1", 1),
        entry(2, "b", "item", "I2", 1),
        entry(3, "c", "item", "I1", 2),
    ];
    let q = SharedQueue::default();
    let stream = LaneFlippingStream::flipping_at(entries, 3);
    let mut r = relay_with(cfg(), stream, src, q.clone());
    r.add_destination(dest("d", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.failed, Some("sync_hub.lane_not_entity"));
    assert_eq!(rep.cursor, 0);
    assert_eq!(r.cursor(), 0);
    assert!(served(&q, "d").is_empty());
    // The refused read remembered no entity, so the same read is refused again unchanged.
    let snap = r.snapshot();
    assert!(snap.lane_of_entity.is_empty());
    assert_eq!(snap.last_txn, None);
    assert_eq!(r.tick(1).unwrap().failed, Some("sync_hub.lane_not_entity"));
}

#[test]
fn a_batch_limit_inside_a_transaction_still_reads_the_whole_transaction() {
    let mut src = Src::default();
    items(&mut src, 4);
    let entries = vec![
        entry(1, "a", "item", "I1", 1),
        entry(2, "a", "item", "I2", 1),
        entry(3, "a", "item", "I3", 1),
        entry(4, "b", "item", "I4", 1),
    ];
    let mut c = cfg();
    c.read_batch = 2;
    let q = SharedQueue::default();
    let mut r = relay_with(c, memory_stream(entries), src, q.clone());
    r.add_destination(dest("d", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.failed, None);
    // The whole transaction, over the batch limit, in one bundle: the contract was kept.
    assert_eq!(rep.read, 3);
    assert_eq!(rep.cursor, 3);
    let all = q.0.borrow().all("d");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].item.bundle.records.len(), 3);
    let rep = r.tick(1).unwrap();
    assert_eq!(rep.failed, None);
    assert_eq!(rep.cursor, 4);
    assert!(r.snapshot().lane_of_entity.contains_key("item"));
}

#[test]
fn a_lane_map_at_its_bound_refuses_a_new_entity() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 1);
    src.put("price", "P1", plain(), 2);
    src.put("tax", "T1", plain(), 3);
    let entries = vec![
        entry(1, "a", "item", "I1", 1),
        entry(2, "b", "price", "P1", 1),
        entry(3, "c", "tax", "T1", 1),
    ];
    let mut c = cfg();
    c.max_entities = 2;
    let q = SharedQueue::default();
    let mut r = relay_with(
        c.clone(),
        memory_stream(entries.clone()),
        src.clone(),
        q.clone(),
    );
    r.add_destination(dest("d", &["item", "price", "tax"]))
        .unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.failed, Some("sync_hub.stream_entity_limit"));
    assert_eq!(r.cursor(), 0);
    assert!(served(&q, "d").is_empty());
    // The bound itself: two known entities are all the read may name.
    let known = vec![entries[0].clone(), entries[1].clone()];
    let q2 = SharedQueue::default();
    let mut r2 = relay_with(c, memory_stream(known), src, q2.clone());
    r2.add_destination(dest("d", &["item", "price"])).unwrap();
    let rep = r2.tick(0).unwrap();
    assert_eq!(rep.failed, None);
    assert_eq!(rep.cursor, 2);
    assert_eq!(r2.snapshot().lane_of_entity.len(), 2);
    assert_eq!(served(&q2, "d").len(), 2);
}

#[test]
fn the_lane_map_and_the_last_transaction_survive_a_restart() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 1);
    src.put("price", "P1", plain(), 2);
    let entries = vec![
        entry(1, "a", "item", "I1", 1),
        entry(2, "b", "price", "P1", 1),
    ];
    let q = SharedQueue::default();
    let mut r1 = relay_with(
        cfg(),
        memory_stream(entries.clone()),
        src.clone(),
        q.clone(),
    );
    r1.add_destination(dest("d", &["item", "price"])).unwrap();
    r1.tick(0).unwrap();
    let snap = r1.snapshot();
    assert_eq!(snap.cursor, 2);
    assert_eq!(snap.last_txn.as_deref(), Some("b"));
    assert_eq!(snap.lane_of_entity.get("item"), Some(&Lane::Bulk));
    assert_eq!(snap.lane_of_entity.get("price"), Some(&Lane::Bulk));

    // A restart whose stream continues transaction b after the cursor: still a split.
    let mut continued = entries.clone();
    continued.push(entry(3, "b", "price", "P1", 2));
    let mut r2 = relay_with(cfg(), memory_stream(continued), src.clone(), q.clone());
    r2.add_destination(dest("d", &["item", "price"])).unwrap();
    r2.restore(snap.clone()).unwrap();
    let rep = r2.tick(1).unwrap();
    assert_eq!(rep.failed, Some("sync_hub.stream_split_txn"));
    assert_eq!(r2.cursor(), 2);

    // A restart whose stream changes the lane of an entity seen before it: still refused.
    let mut flipped = entries.clone();
    let mut e = entry(3, "c", "item", "I1", 2);
    e.lane = Lane::Mandatory;
    flipped.push(e);
    let mut r3 = relay_with(cfg(), memory_stream(flipped), src, q.clone());
    r3.add_destination(dest("d", &["item", "price"])).unwrap();
    r3.restore(snap).unwrap();
    let rep = r3.tick(2).unwrap();
    assert_eq!(rep.failed, Some("sync_hub.lane_not_entity"));
    assert_eq!(r3.cursor(), 2);
}

/// A stream that returns the window in reverse order when it is re-read: the double for proving
/// that the catch-up read is checked too. A persistent adapter would not ship it.
struct ReorderingStream {
    entries: Vec<StreamEntry>,
    /// A read that starts after exactly this position comes back reversed.
    reverse_after: u64,
}

impl HubStream for ReorderingStream {
    fn read_after(&self, after: u64, max: usize) -> Result<Vec<StreamEntry>, HubError> {
        let mut out = read_whole(&self.entries, after, max);
        if after == self.reverse_after {
            out.reverse();
        }
        Ok(out)
    }

    fn head(&self) -> Result<u64, HubError> {
        Ok(self.entries.last().map_or(0, |e| e.position))
    }
}

#[test]
fn a_catch_up_read_that_breaks_the_contract_queues_nothing_and_keeps_the_destination_behind() {
    let mut src = Src::default();
    items(&mut src, 3);
    let entries = vec![
        entry(1, "a", "item", "I1", 1),
        entry(2, "b", "item", "I2", 1),
        entry(3, "c", "item", "I3", 1),
    ];
    let mut c = cfg();
    c.read_batch = 3;
    c.max_bundles_per_tick_per_dest = 1;
    let q = SharedQueue::default();
    let stream = ReorderingStream {
        entries,
        reverse_after: 1,
    };
    let mut r = relay_with(c, stream, src, q.clone());
    r.add_destination(dest("d", &["item"])).unwrap();
    r.tick(0).unwrap();
    // The second group did not fit the bundle cap: the destination is behind from position 2.
    assert_eq!(r.behind_from("d"), Some(2));
    let queued = served(&q, "d");
    assert_eq!(queued.len(), 1);
    // The catch-up re-read of the window starts after position 1: it comes back out of order.
    let second = r.tick(1).unwrap();
    assert_eq!(
        second.destinations["d"].failed,
        Some("sync_hub.stream_disorder"),
        "the catch-up read is checked as well"
    );
    assert_eq!(served(&q, "d"), queued, "nothing was queued from it");
    assert_eq!(r.behind_from("d"), Some(2), "the destination stays behind");
}

#[test]
fn every_new_code_is_in_the_catalogue() {
    let toml = include_str!("../../spot-errors/catalogue/sync_hub.toml");
    for code in [
        spot_sync_hub::STREAM_SPLIT_TXN,
        spot_sync_hub::STREAM_ENTITY_LIMIT,
        spot_sync_hub::LANE_NOT_ENTITY,
    ] {
        assert!(toml.contains(&format!("code = \"{code}\"")), "{code}");
    }
}
