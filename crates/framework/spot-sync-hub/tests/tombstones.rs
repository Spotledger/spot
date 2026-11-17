//! Deletions travel (WP-SYN-02.T6): a deleted entry becomes a tombstone in the bundle, built from
//! the entry itself, under the same tenant, scope and extras rules as any other entry.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use serde_json::Value;
use spot_sync_closure::RecordKey;
use spot_sync_hub::{HubRelay, MemoryStream, StreamEntry};

type Relay = HubRelay<SharedQueue, MemoryStream, Src>;

fn deleted(mut e: StreamEntry) -> StreamEntry {
    e.deleted = true;
    e
}

fn relay(entries: Vec<StreamEntry>, src: Src, q: &SharedQueue) -> Relay {
    let mut s = MemoryStream::new();
    for e in entries {
        s.append(e);
    }
    HubRelay::new(cfg(), s, q.clone(), src, Box::new(Refs)).unwrap()
}

/// `(key, position, deleted)` of everything queued for `dest`, in serving order.
fn flags(q: &SharedQueue, dest: &str) -> Vec<(RecordKey, u64, bool)> {
    q.0.borrow()
        .all(dest)
        .iter()
        .flat_map(|p| {
            p.item
                .bundle
                .records
                .iter()
                .map(|r| (r.key.clone(), r.position, r.deleted))
        })
        .collect()
}

#[test]
fn create_update_delete_travel_in_order() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 2);
    let q = SharedQueue::default();
    let mut r = relay(
        vec![
            entry(1, "a", "item", "I1", 1),
            entry(2, "b", "item", "I1", 2),
            deleted(entry(3, "c", "item", "I1", 3)),
        ],
        src,
        &q,
    );
    r.add_destination(dest("d", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.destinations["d"].failed, None);
    assert_eq!(
        flags(&q, "d"),
        vec![
            (k("item", "I1"), 2, false),
            (k("item", "I1"), 2, false),
            (k("item", "I1"), 3, true),
        ]
    );
}

#[test]
fn a_deleted_entry_is_never_asked_of_the_image_source() {
    // The source holds nothing for the record: a read would fail with `image_missing`.
    let q = SharedQueue::default();
    let mut r = relay(
        vec![deleted(entry(1, "a", "item", "GONE", 5))],
        Src::default(),
        &q,
    );
    r.add_destination(dest("d", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.destinations["d"].failed, None);
    assert_eq!(rep.destinations["d"].bundles_queued, 1);
    let all = q.0.borrow().all("d");
    let record = &all[0].item.bundle.records[0];
    assert!(record.deleted);
    assert_eq!(record.key, k("item", "GONE"));
    assert_eq!(record.position, 1);
    assert_eq!(record.image, Value::Null);
    assert_eq!(all[0].item.bundle.bytes, 0);
}

#[test]
fn a_delete_for_a_record_outside_the_scope_sends_nothing() {
    let q = SharedQueue::default();
    let mut r = relay(
        vec![deleted(entry(1, "a", "price", "P1", 2))],
        Src::default(),
        &q,
    );
    r.add_destination(dest("d", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.destinations["d"].failed, None);
    assert!(flags(&q, "d").is_empty());
    assert_eq!(rep.cursor, 1);
}

#[test]
fn delete_then_create_with_the_same_id_sends_both_in_position_order() {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 4);
    let q = SharedQueue::default();
    let mut r = relay(
        vec![
            deleted(entry(3, "a", "item", "I1", 2)),
            entry(4, "b", "item", "I1", 1),
        ],
        src,
        &q,
    );
    r.add_destination(dest("d", &["item"])).unwrap();
    r.tick(0).unwrap();
    assert_eq!(
        flags(&q, "d"),
        vec![(k("item", "I1"), 3, true), (k("item", "I1"), 4, false)]
    );
}

#[test]
fn a_tombstone_of_a_delivered_extra_is_sent_and_the_key_leaves_the_extras_set() {
    let mut src = Src::default();
    src.put("master", "M1", plain(), 1);
    src.put("order", "O1", linking(&["M1"]), 2);
    let q = SharedQueue::default();
    let mut r = relay(vec![entry(2, "a", "order", "O1", 1)], src, &q);
    r.add_destination(dest("d", &["order"])).unwrap();
    r.tick(0).unwrap();
    assert!(r.extras("d").unwrap().contains(&k("master", "M1")));
    // The master is out of the destination's scope; its delete still reaches it as an extra.
    r.stream_mut()
        .append(deleted(entry(5, "b", "master", "M1", 2)));
    let rep = r.tick(1).unwrap();
    assert_eq!(rep.destinations["d"].failed, None);
    let sent = flags(&q, "d");
    assert!(sent.contains(&(k("master", "M1"), 5, true)));
    assert!(!r.extras("d").unwrap().contains(&k("master", "M1")));
}

#[test]
fn a_tombstone_of_a_record_never_an_extra_and_out_of_scope_is_not_sent() {
    let q = SharedQueue::default();
    let mut r = relay(
        vec![deleted(entry(1, "a", "master", "M9", 2))],
        Src::default(),
        &q,
    );
    r.add_destination(dest("d", &["order"])).unwrap();
    r.tick(0).unwrap();
    assert!(flags(&q, "d").is_empty());
    assert!(!r.extras("d").unwrap().contains(&k("master", "M9")));
}

#[test]
fn a_tombstone_of_another_tenant_is_dropped_for_everyone() {
    let mut foreign = deleted(entry(1, "a", "item", "I1", 2));
    foreign.tenant = "other".to_owned();
    let q = SharedQueue::default();
    let mut r = relay(vec![foreign], Src::default(), &q);
    r.add_destination(dest("d", &["item"])).unwrap();
    let rep = r.tick(0).unwrap();
    assert_eq!(rep.foreign_dropped, 1);
    assert!(flags(&q, "d").is_empty());
}

#[test]
fn a_tombstone_the_destination_does_not_get_back_from_origin() {
    // The up direction: the delete came from the destination itself and is never sent back.
    let mut e = deleted(entry(1, "a", "item", "I1", 2));
    e.origin = Some("d".to_owned());
    let q = SharedQueue::default();
    let mut r = relay(vec![e], Src::default(), &q);
    r.add_destination(dest("d", &["item"])).unwrap();
    r.tick(0).unwrap();
    assert!(flags(&q, "d").is_empty());
}

#[test]
fn a_replayed_tombstone_push_is_a_duplicate_not_a_second_item() {
    let entries = || vec![deleted(entry(1, "a", "item", "I1", 2))];
    let q = SharedQueue::default();
    let mut first = relay(entries(), Src::default(), &q);
    first.add_destination(dest("d", &["item"])).unwrap();
    let before = first.snapshot();
    first.tick(0).unwrap();
    assert_eq!(flags(&q, "d").len(), 1);

    // A restart from a snapshot taken before the tick replays the push over the durable queue.
    let mut second = relay(entries(), Src::default(), &q);
    second.add_destination(dest("d", &["item"])).unwrap();
    second.restore(before).unwrap();
    let rep = second.tick(1).unwrap();
    assert_eq!(rep.destinations["d"].bundles_duplicate, 1);
    assert_eq!(rep.destinations["d"].bundles_queued, 0);
    assert_eq!(flags(&q, "d").len(), 1);
}

#[test]
fn the_stream_entry_deleted_flag_defaults_to_false_when_absent() {
    let mut value = serde_json::to_value(entry(1, "a", "item", "I1", 1)).unwrap();
    value.as_object_mut().unwrap().remove("deleted");
    let back: StreamEntry = serde_json::from_value(value).unwrap();
    assert!(!back.deleted);
}
