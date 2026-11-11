//! A held record must never leave the hub by the side door (ADR-066 section 5): the closure does
//! not pull it in as an extra, and a record that needs it is withheld with it until the node can
//! decode the dependency.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use spot_sync_closure::{RecordKey, Role};
use spot_sync_hub::{
    Destination, DestinationScope, HubRelay, MemoryStream, QueuedBundle, StreamEntry,
};
use spot_sync_obligations::Pending;

type Relay = HubRelay<SharedQueue, MemoryStream, Src>;

fn relay_with(stream: MemoryStream, src: Src, queue: SharedQueue) -> Relay {
    HubRelay::new(cfg(), stream, queue, src, Box::new(Refs)).unwrap()
}

fn stream_of(entries: Vec<StreamEntry>) -> MemoryStream {
    let mut s = MemoryStream::new();
    for e in entries {
        s.append(e);
    }
    s
}

/// An entry of `entity` whose event is schema major `major`.
fn at(position: u64, txn: &str, entity: &str, id: &str, major: u32) -> StreamEntry {
    let mut e = entry(position, txn, entity, id, 1);
    e.major = major;
    e
}

fn dest_reporting(id: &str, entities: &[&str], majors: &[(&str, &[u32])]) -> Destination {
    Destination::new(
        id,
        Box::new(Scope::of(entities)),
        Box::new(StripSecret),
        report(majors),
    )
}

fn queued(queue: &SharedQueue, dest: &str) -> Vec<Pending<QueuedBundle>> {
    queue.0.borrow().all(dest)
}

#[test]
fn a_root_whose_dependency_is_held_is_withheld_with_it() {
    let mut src = Src::default();
    src.put("master", "M1", plain(), 2);
    src.put("order", "O1", linking(&["M1"]), 3);
    let stream = stream_of(vec![
        at(2, "a", "master", "M1", 2),
        entry(3, "b", "order", "O1", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(stream, src, q.clone());
    r.add_destination(dest("d", &["order", "master"])).unwrap();
    let rep = r.tick(0).unwrap();
    // The held record and the record that needs it are both withheld: nothing is queued.
    assert_eq!(rep.destinations["d"].held, 1);
    assert_eq!(rep.destinations["d"].held_by_dependency, 1);
    assert!(queued(&q, "d").is_empty());
    assert_eq!(r.held_positions("d"), vec![2, 3]);
    assert_eq!(
        r.held_entries("d"),
        vec![(2, k("master", "M1")), (3, k("order", "O1"))]
    );
}

#[test]
fn the_upgrade_releases_the_dependency_before_the_record_that_needs_it() {
    let mut src = Src::default();
    src.put("master", "M1", plain(), 2);
    src.put("order", "O1", linking(&["M1"]), 3);
    let stream = stream_of(vec![
        at(2, "a", "master", "M1", 2),
        entry(3, "b", "order", "O1", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(stream, src, q.clone());
    r.add_destination(dest("d", &["order", "master"])).unwrap();
    r.tick(0).unwrap();
    assert!(queued(&q, "d").is_empty());

    let rel = r
        .on_node_upgraded("d", report(&[("order", &[1]), ("master", &[1, 2])]))
        .unwrap();
    assert_eq!(rel.released, 1);
    assert_eq!(rel.released_by_dependency, 1);
    assert_eq!(rel.bundles_queued, 2);
    assert!(rel.missing.is_empty());
    assert!(r.held_positions("d").is_empty());

    let all = queued(&q, "d");
    let positions: Vec<u64> = all.iter().map(|p| p.position).collect();
    assert_eq!(positions, vec![2, 3]);
    assert!(all.iter().all(|p| p.item.redelivery));
    // The dependency goes first, then the dependent exactly once, in a bundle that carries its
    // dependency so it can be applied on its own.
    assert_eq!(all[0].item.bundle.records.len(), 1);
    assert_eq!(all[0].item.bundle.records[0].key, k("master", "M1"));
    let second: Vec<(RecordKey, Role)> = all[1]
        .item
        .bundle
        .records
        .iter()
        .map(|rec| (rec.key.clone(), rec.role))
        .collect();
    assert_eq!(
        second,
        vec![
            (k("master", "M1"), Role::Extra),
            (k("order", "O1"), Role::Root)
        ]
    );
    // Redeliveries are not trimmed by a prefix ack, and go away on their own ack.
    r.on_ack("d", 3).unwrap();
    assert_eq!(queued(&q, "d").len(), 2);
    r.on_redelivery_ack("d", &[2, 3]).unwrap();
    assert!(queued(&q, "d").is_empty());
}

#[test]
fn a_held_record_needed_by_two_roots_withholds_both() {
    let mut src = Src::default();
    src.put("master", "M1", plain(), 2);
    src.put("order", "O1", linking(&["M1"]), 3);
    src.put("order", "O2", linking(&["M1"]), 4);
    let stream = stream_of(vec![
        at(2, "a", "master", "M1", 2),
        entry(3, "b", "order", "O1", 1),
        entry(4, "c", "order", "O2", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(stream, src, q.clone());
    r.add_destination(dest("d", &["order", "master"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.destinations["d"].held, 1);
    assert_eq!(rep.destinations["d"].held_by_dependency, 2);
    assert!(queued(&q, "d").is_empty());
    assert_eq!(
        r.held_entries("d"),
        vec![
            (2, k("master", "M1")),
            (3, k("order", "O1")),
            (4, k("order", "O2")),
        ]
    );
}

#[test]
fn a_held_record_is_still_sent_to_a_destination_that_can_decode_it() {
    let mut src = Src::default();
    src.put("master", "M1", plain(), 2);
    src.put("order", "O1", linking(&["M1"]), 3);
    let stream = stream_of(vec![
        at(2, "a", "master", "M1", 2),
        entry(3, "b", "order", "O1", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(stream, src, q.clone());
    r.add_destination(dest("old", &["order", "master"]))
        .unwrap();
    r.add_destination(dest_reporting(
        "new",
        &["order", "master"],
        &[("order", &[1]), ("master", &[1, 2])],
    ))
    .unwrap();
    let rep = r.tick(0).unwrap();
    // The old node holds both; the new one has not held anything and gets the record.
    assert_eq!(rep.destinations["old"].held, 1);
    assert_eq!(rep.destinations["old"].held_by_dependency, 1);
    assert!(queued(&q, "old").is_empty());
    assert_eq!(rep.destinations["new"].held, 0);
    assert_eq!(rep.destinations["new"].held_by_dependency, 0);
    let served: Vec<RecordKey> = queued(&q, "new")
        .iter()
        .flat_map(|p| p.item.bundle.records.iter().map(|rec| rec.key.clone()))
        .collect();
    assert!(served.contains(&k("master", "M1")));
    assert!(served.contains(&k("order", "O1")));
}

#[test]
fn a_dependent_waits_for_every_held_record_it_needs() {
    let mut src = Src::default();
    src.put("master", "M1", plain(), 2);
    src.put("master", "M2", plain(), 3);
    src.put("order", "O1", linking(&["M1", "M2"]), 4);
    let stream = stream_of(vec![
        at(2, "a", "master", "M1", 2),
        at(3, "b", "master", "M2", 3),
        entry(4, "c", "order", "O1", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(stream, src, q.clone());
    r.add_destination(dest("d", &["order", "master"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.destinations["d"].held, 2);
    assert_eq!(rep.destinations["d"].held_by_dependency, 1);

    // The node learns major 2 only: M1 goes, M2 and its dependent stay held.
    let rel = r
        .on_node_upgraded("d", report(&[("order", &[1]), ("master", &[1, 2])]))
        .unwrap();
    assert_eq!(rel.released, 1);
    assert_eq!(rel.released_by_dependency, 0);
    assert_eq!(r.held_positions("d"), vec![3, 4]);
    let served: Vec<RecordKey> = queued(&q, "d")
        .iter()
        .flat_map(|p| p.item.bundle.records.iter().map(|rec| rec.key.clone()))
        .collect();
    assert_eq!(served, vec![k("master", "M1")]);

    // Then major 3: M2 and the order go, dependency first.
    let rel = r
        .on_node_upgraded("d", report(&[("order", &[1]), ("master", &[1, 2, 3])]))
        .unwrap();
    assert_eq!(rel.released, 1);
    assert_eq!(rel.released_by_dependency, 1);
    assert!(r.held_positions("d").is_empty());
    let all = queued(&q, "d");
    let positions: Vec<u64> = all.iter().map(|p| p.position).collect();
    assert_eq!(positions, vec![2, 3, 4]);
}

/// A destination whose entitlement can be revoked while entries are withheld from it.
struct Revocable(std::rc::Rc<std::cell::Cell<bool>>);

impl DestinationScope for Revocable {
    fn covers(&self, entry: &StreamEntry) -> bool {
        self.0.get() && (entry.key.entity == "order" || entry.key.entity == "master")
    }
    fn covers_key(&self, key: &RecordKey) -> bool {
        self.0.get() && (key.entity == "order" || key.entity == "master")
    }
    fn owns(&self, _key: &RecordKey) -> bool {
        false
    }
}

#[test]
fn a_withheld_dependent_is_not_released_after_the_scope_was_revoked() {
    let mut src = Src::default();
    src.put("master", "M1", plain(), 2);
    src.put("order", "O1", linking(&["M1"]), 3);
    let stream = stream_of(vec![
        at(2, "a", "master", "M1", 2),
        entry(3, "b", "order", "O1", 1),
    ]);
    let q = SharedQueue::default();
    let mut r = relay_with(stream, src, q.clone());
    let live = std::rc::Rc::new(std::cell::Cell::new(true));
    r.add_destination(Destination::new(
        "d",
        Box::new(Revocable(live.clone())),
        Box::new(StripSecret),
        v1(&["order", "master"]),
    ))
    .unwrap();
    r.tick(0).unwrap();
    assert_eq!(r.held_positions("d"), vec![2, 3]);
    assert!(queued(&q, "d").is_empty());
    live.set(false);
    let rel = r
        .on_node_upgraded("d", report(&[("order", &[1]), ("master", &[1, 2])]))
        .unwrap();
    assert_eq!(rel.bundles_queued, 0);
    assert_eq!(rel.pending, 0);
    assert!(r.held_positions("d").is_empty());
    assert!(queued(&q, "d").is_empty());
}

#[test]
fn the_dependency_code_is_in_the_catalogue() {
    let toml = include_str!("../../spot-errors/catalogue/sync_hub.toml");
    assert!(
        toml.contains(&format!("code = \"{}\"", spot_sync_hub::HELD_BY_DEPENDENCY)),
        "{}",
        spot_sync_hub::HELD_BY_DEPENDENCY
    );
}
