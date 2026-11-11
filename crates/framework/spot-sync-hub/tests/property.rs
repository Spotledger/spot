//! Property: for any entry sequence and scope, what each destination is served is in position
//! order per record key, and never contains a record of another tenant.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use proptest::prelude::*;
use spot_sync_hub::{Destination, HubRelay, MemoryStream};
use spot_sync_obligations::Lane;
use std::collections::{BTreeMap, BTreeSet};

proptest! {
    #[test]
    fn per_record_position_order_holds_for_any_stream_and_scope(
        picks in proptest::collection::vec((0usize..4, 0usize..5, any::<bool>()), 1..60),
        batch in 1usize..9,
        cap in 1usize..6,
        scope_mask in 1u8..8,
    ) {
        let entities = ["item", "price", "tax"];
        let mut src = Src::default();
        let mut stream = MemoryStream::new();
        for (i, (e, id, foreign)) in picks.iter().enumerate() {
            let position = u64::try_from(i + 1).unwrap();
            let entity = entities[*e % 3];
            let id = if *foreign { format!("F{id}") } else { format!("R{id}") };
            src.put(entity, &id, plain(), position);
            let mut en = entry(position, &format!("t{position}"), entity, &id, position);
            en.lane = if entity == "tax" { Lane::Mandatory } else { Lane::Bulk };
            if *foreign {
                en.tenant = "other".to_owned();
            }
            stream.append(en);
        }
        let mut c = cfg();
        c.read_batch = batch;
        c.max_bundles_per_tick_per_dest = cap;
        let q = SharedQueue::default();
        let mut r = HubRelay::new(c, stream, q.clone(), src, Box::new(Refs)).unwrap();
        let wanted: Vec<&str> = entities
            .iter()
            .enumerate()
            .filter(|(i, _)| scope_mask & (1 << i) != 0)
            .map(|(_, e)| *e)
            .collect();
        r.add_destination(dest("d", &wanted)).unwrap();
        for t in 0..200 {
            r.tick(t).unwrap();
            if r.cursor() >= u64::try_from(picks.len()).unwrap() && r.behind_from("d").is_none() {
                break;
            }
        }
        let mut last: BTreeMap<_, u64> = BTreeMap::new();
        for (key, position) in served(&q, "d") {
            prop_assert!(wanted.contains(&key.entity.as_str()));
            let prev = last.insert(key.clone(), position).unwrap_or(0);
            prop_assert!(position >= prev, "{key:?}: {position} after {prev}");
        }
        // Nothing of the other tenant was served, as a root or as an extra.
        for p in q.0.borrow().all("d") {
            for rec in &p.item.bundle.records {
                prop_assert!(!rec.key.id.starts_with('F'), "{:?}", rec.key);
            }
        }
    }
}

proptest! {
    /// A stream that keeps the contract is never refused: whole transactions, one stable lane per
    /// entity, ascending positions. Whatever the shapes, the forward and catch-up reads of every
    /// tick pass the check.
    #[test]
    fn a_compliant_stream_is_never_refused(
        txns in proptest::collection::vec((0usize..4, 1usize..5), 1..12),
        lanes in proptest::collection::vec(any::<bool>(), 4),
        batch in 1usize..7,
        cap in 1usize..5,
        scope_mask in 1u8..16,
    ) {
        let entities = ["item", "price", "tax", "master"];
        let mut src = Src::default();
        let mut stream = MemoryStream::new();
        let mut used: BTreeSet<&str> = BTreeSet::new();
        let mut position = 0u64;
        for (i, (entity, size)) in txns.iter().enumerate() {
            let entity = entities[entity % 4];
            used.insert(entity);
            // The lane is a function of the entity, never of the entry.
            let lane = if lanes[entity.len() % 4] {
                Lane::Mandatory
            } else {
                Lane::Bulk
            };
            for _ in 0..*size {
                position += 1;
                let id = format!("R{position}");
                src.put(entity, &id, plain(), position);
                let mut e = entry(position, &format!("t{i}"), entity, &id, 1);
                e.lane = lane;
                stream.append(e);
            }
        }
        let mut c = cfg();
        c.read_batch = batch;
        c.max_bundles_per_tick_per_dest = cap;
        let q = SharedQueue::default();
        let mut r = HubRelay::new(c, stream, q.clone(), src, Box::new(Refs)).unwrap();
        let wanted: Vec<&str> = entities
            .iter()
            .enumerate()
            .filter(|(i, _)| scope_mask & (1 << i) != 0)
            .map(|(_, e)| *e)
            .collect();
        r.add_destination(dest("d", &wanted)).unwrap();
        for t in 0..300 {
            let rep = r.tick(t).unwrap();
            prop_assert!(rep.failed.is_none(), "refused with {:?}", rep.failed);
            prop_assert!(rep.destinations["d"].failed.is_none());
            if r.cursor() >= position && r.behind_from("d").is_none() {
                break;
            }
        }
        prop_assert_eq!(r.cursor(), position);
        // What the relay remembered matches the stream it read.
        let last = format!("t{}", txns.len() - 1);
        let snap = r.snapshot();
        prop_assert_eq!(snap.last_txn.as_deref(), Some(last.as_str()));
        prop_assert_eq!(snap.lane_of_entity.len(), used.len());
    }
}

proptest! {
    /// A held record never leaves the hub by the side door: no bundle queued for a destination
    /// carries a record the destination cannot decode yet, in any role, and no bundle is a
    /// dependent that waits for such a record either (ADR-066 section 5).
    #[test]
    fn no_queued_bundle_ever_carries_a_held_record(
        n_docs in 1usize..8,
        n_masters in 1usize..6,
        links in proptest::collection::vec(proptest::collection::vec(0usize..6, 0..3), 8),
        master_majors in proptest::collection::vec(1u32..3, 6),
        scope_mask in 0u8..4,
    ) {
        let mut src = Src::default();
        let mut entries: Vec<spot_sync_hub::StreamEntry> = Vec::new();
        let mut position = 0u64;
        for i in 0..n_masters {
            position += 1;
            let id = format!("M{i}");
            src.put("master", &id, plain(), position);
            let mut e = entry(position, &format!("m{i}"), "master", &id, 1);
            e.major = master_majors[i % 6];
            entries.push(e);
        }
        for j in 0..n_docs {
            position += 1;
            let id = format!("D{j}");
            let targets: Vec<String> = links[j % 8]
                .iter()
                .filter(|m| **m < n_masters)
                .map(|m| format!("M{m}"))
                .collect();
            let l: Vec<&str> = targets.iter().map(String::as_str).collect();
            src.put("doc", &id, linking(&l), position);
            entries.push(entry(position, &format!("d{j}"), "doc", &id, 1));
        }
        let mut stream = MemoryStream::new();
        for e in entries {
            stream.append(e);
        }
        let q = SharedQueue::default();
        let mut r = HubRelay::new(cfg(), stream, q.clone(), src, Box::new(Refs)).unwrap();
        let mut entities = vec!["doc"];
        if scope_mask & 2 != 0 {
            entities.push("master");
        }
        r.add_destination(Destination::new(
            "d",
            Box::new(Scope::of(&entities)),
            Box::new(StripSecret),
            report(&[("doc", &[1]), ("master", &[1])]),
        ))
        .unwrap();
        r.tick(0).unwrap();

        let held = r.held_entries("d");
        for pending in q.0.borrow().all("d") {
            prop_assert!(!pending.item.bundle.records.is_empty());
            for rec in &pending.item.bundle.records {
                for (held_position, key) in &held {
                    prop_assert!(
                        !(*key == rec.key && pending.position >= *held_position),
                        "{:?} was queued at {} although it is held from {}",
                        rec.key, pending.position, held_position
                    );
                }
            }
        }
    }
}
