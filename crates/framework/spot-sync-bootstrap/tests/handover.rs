#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The handover: snapshot at P, then the incremental stream from P + 1.

mod common;

use common::*;
use serde_json::{Value, json};
use spot_sync_bootstrap::{Bootstrap, Budget, RecordKey, StartOptions};
use spot_sync_catchup::{
    CatchUpError, ClassCounts, Event, EventClass, GapSummary, Mode, Payload, RecordId, References,
    Settings, StaticTopics, plan, scope_for_node,
};
use spot_sync_closure::Limits;
use std::collections::BTreeMap;

struct NoLinks;

impl References for NoLinks {
    fn references(&self, _: &RecordId, _: &Payload) -> Result<Vec<RecordId>, CatchUpError> {
        Ok(Vec::new())
    }
}

/// The node's applied state, with the version rule and a detector for a lower version applied
/// after a higher one.
#[derive(Default)]
struct Node {
    state: BTreeMap<RecordKey, (u64, Value)>,
    regressions: usize,
}

impl Node {
    fn apply(&mut self, key: RecordKey, version: u64, image: Value) {
        match self.state.get(&key) {
            Some((have, _)) if version < *have => self.regressions += 1,
            Some((have, _)) if version == *have => {}
            _ => {
                self.state.insert(key, (version, image));
            }
        }
    }
}

fn state_event(position: u64, id: &str, version: u64) -> Event {
    let image = json!({ "id": id, "links": [], "v": version });
    Event::new(
        position,
        "doc_state",
        Payload::new(json!({ "id": id, "version": version, "image": image }).to_string()),
    )
    .with_subject(RecordId::new("Customer", id))
    .with_subject_version(version)
}

#[test]
fn snapshot_then_incremental_equals_the_hub_state() {
    // The hub's history. Records change before, at and after the snapshot position.
    let history: Vec<(u64, &str, u64)> = vec![
        (10, "a", 1),
        (20, "b", 1),
        (300, "a", 2),
        (POSITION, "b", 2), // at P: in the snapshot
        (POSITION - 1, "c", 1),
        (POSITION + 1, "a", 3), // right after P
        (POSITION + 7, "d", 1), // new after P
        (POSITION + 9, "b", 3),
    ];
    let head = POSITION + 9;

    let mut at_p: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    let mut at_head: BTreeMap<&str, u64> = BTreeMap::new();
    let mut sorted = history.clone();
    sorted.sort_by_key(|(p, _, _)| *p);
    for (p, id, v) in &sorted {
        if *p <= POSITION {
            at_p.insert(id, (*v, *p));
        }
        at_head.insert(id, *v);
    }
    let snapshot_records = at_p
        .iter()
        .map(|(id, (v, _))| {
            let mut r = rec("Customer", id, *v, &[]);
            r.image = json!({ "id": id, "links": [], "v": v });
            r
        })
        .collect();
    let src = source(snapshot_records);

    let limits = Limits::default();
    let mut session = Bootstrap::start(
        &src,
        seams(&SCOPE),
        limits,
        &node("n1"),
        spot_sync_bootstrap::HubHead {
            generation: GENERATION,
            head_position: head,
        },
        StartOptions::default(),
    )
    .unwrap();
    let mut me = Node::default();
    let mut resume = None;
    while let Some(c) = session.next_chunk(Budget::of(&limits)).unwrap() {
        for b in c.bundles {
            for r in b.records {
                me.apply(r.key, r.version, r.image);
            }
        }
        resume = c.resume_from;
    }
    let resume = resume.unwrap();
    assert_eq!(resume, POSITION + 1);

    // The incremental plan from P + 1. The event list deliberately includes events at or before P:
    // they overlap the snapshot and must not come again.
    let events: Vec<Event> = sorted
        .iter()
        .map(|(p, id, v)| state_event(*p, id, *v))
        .collect();
    let in_gap = events.iter().filter(|e| e.position >= resume).count() as u64;
    let summary = GapSummary {
        applied_prefix: resume - 1,
        stream_end: head,
        hot_retention_floor: 0,
        in_scope_event_count: in_gap,
        class_counts: ClassCounts {
            state: in_gap,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_records: 3,
        estimated_coalesced_bytes: 300,
        estimated_snapshot_bytes: 10_000,
        node_is_new_or_resync: false,
    };
    let topics = StaticTopics::new().with("doc_state", EventClass::State);
    let gap = scope_for_node(&events, "n1", &topics);
    let catch_up = plan(&gap, &summary, &Settings::default(), &topics, &NoLinks).unwrap();
    assert_eq!(catch_up.mode, Mode::Incremental);
    let planned: Vec<u64> = catch_up
        .bulk
        .iter()
        .flat_map(|b| b.events.iter().map(|e| e.position))
        .collect();
    assert_eq!(planned, vec![POSITION + 1, POSITION + 7, POSITION + 9]);
    for e in catch_up.bulk.iter().flat_map(|b| &b.events) {
        let body: Value = serde_json::from_str(e.payload.as_str()).unwrap();
        let id = body["id"].as_str().unwrap();
        me.apply(
            RecordKey::new("Customer", id),
            body["version"].as_u64().unwrap(),
            body["image"].clone(),
        );
    }

    assert_eq!(me.regressions, 0);
    let got: BTreeMap<String, u64> = me
        .state
        .iter()
        .map(|(k, (v, _))| (k.id.clone(), *v))
        .collect();
    let want: BTreeMap<String, u64> = at_head.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
    assert_eq!(got, want);
    for (k, (v, image)) in &me.state {
        assert_eq!(image["v"], json!(v), "{k:?} image matches its version");
    }
}

#[test]
fn replaying_an_overlapping_event_is_harmless() {
    // The stream repeats the snapshot's own version: applied idempotently, never a regression.
    let mut me = Node::default();
    me.apply(RecordKey::new("Customer", "a"), 2, json!({"v": 2}));
    me.apply(RecordKey::new("Customer", "a"), 2, json!({"v": 2}));
    assert_eq!(me.regressions, 0);
    me.apply(RecordKey::new("Customer", "a"), 1, json!({"v": 1}));
    assert_eq!(me.regressions, 1);
}
