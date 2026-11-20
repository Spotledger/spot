//! Serving and applying chunks over a snapshot in memory: every record arrives once, a cut resumes
//! at the checkpoint, a tampered or out-of-order chunk is refused with nothing applied, an unknown
//! scope or an unverified snapshot is refused, and a catch-up plan is served piece by piece.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use spot_journal_snapshot::{ImageRecord, SnapshotInfo};
use spot_sync_bootstrap::{
    Bundle, ChunkRecord, HubHead, MemorySource, NodeState, ScopeView, SnapshotSource,
};
use spot_sync_catchup::{CatchUpPlan, ClassCounts, Event, GapSummary, Mode, Payload};
use spot_sync_catchup_host::{
    Applied, ApplyTarget, CatchupConfig, ChunkApplier, ChunkServer, Director, MemoryProgress,
    NodeSeams, ProgressStore, Route, SeamsProvider, ServeError, SnapshotProvider,
};
use spot_sync_closure::{ClosureError, Limits, NoRedaction, RecordKey, References};

const TENANT: &str = "acme";
const NODE: &str = "branch-1";
const POSITION: u64 = 100;

struct Everything;

impl ScopeView for Everything {
    fn covers(&self, _key: &RecordKey, _image: &Value) -> bool {
        true
    }
    fn fingerprint(&self) -> String {
        "everything".to_owned()
    }
}

struct NoLinks;

impl References for NoLinks {
    fn references(&self, _entity: &str, _image: &Value) -> Result<Vec<RecordKey>, ClosureError> {
        Ok(Vec::new())
    }
}

struct Seams {
    known: bool,
}

impl SeamsProvider for Seams {
    fn for_node(&self, node: &str) -> Option<NodeSeams> {
        (self.known && node == NODE).then(|| NodeSeams {
            scope: Box::new(Everything),
            references: Box::new(NoLinks),
            redactor: Box::new(NoRedaction),
        })
    }
}

struct Snapshot {
    verified: bool,
}

fn records(count: u64) -> Vec<ImageRecord> {
    (0..count)
        .map(|n| ImageRecord {
            entity: "item".to_owned(),
            id: format!("item-{n:04}"),
            version: 1,
            entity_schema: 1,
            image: json!({"name": format!("Item {n}")}),
        })
        .collect()
}

impl SnapshotProvider for Snapshot {
    fn open(&self) -> Result<Box<dyn SnapshotSource>, ServeError> {
        if !self.verified {
            return Err(ServeError::NoVerifiedSnapshot);
        }
        let info = SnapshotInfo {
            position: POSITION,
            generation: 1,
            verified: true,
            taken_at_millis: 0,
        };
        Ok(Box::new(MemorySource::new(TENANT, info, records(7))))
    }
}

/// Records what the apply path was asked to apply, by record key.
#[derive(Default)]
struct Target {
    applied: Mutex<Vec<String>>,
    events: Mutex<Vec<u64>>,
    fail: bool,
}

impl ApplyTarget for Target {
    fn apply_bundle(&self, bundle: &Bundle) -> Result<(), ServeError> {
        if self.fail {
            return Err(ServeError::ApplyFailed("apply.refused"));
        }
        let mut applied = self.applied.lock().unwrap();
        for ChunkRecord { key, .. } in &bundle.records {
            applied.push(key.id.clone());
        }
        Ok(())
    }
    fn apply_events(&self, events: &[Event]) -> Result<(), ServeError> {
        self.events
            .lock()
            .unwrap()
            .extend(events.iter().map(|e| e.position));
        Ok(())
    }
}

struct Rig {
    server: ChunkServer,
    store: Arc<MemoryProgress>,
}

fn rig(known: bool, verified: bool) -> Rig {
    let store = Arc::new(MemoryProgress::default());
    let director = Arc::new(Director::new(&CatchupConfig::default(), store.clone()).unwrap());
    // A new node: the route admits it and stores a bootstrap in progress.
    let new_node = GapSummary {
        applied_prefix: 0,
        stream_end: POSITION + 20,
        hot_retention_floor: 0,
        in_scope_event_count: 0,
        class_counts: ClassCounts {
            state: 0,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_records: 0,
        estimated_coalesced_bytes: 0,
        estimated_snapshot_bytes: 1_000_000,
        node_is_new_or_resync: true,
    };
    assert!(matches!(
        director.route(NODE, &new_node, 0).unwrap(),
        Route::Bootstrap { .. }
    ));
    let limits = Limits {
        max_records: 2,
        ..Limits::default()
    };
    Rig {
        server: ChunkServer::new(
            director,
            Arc::new(Snapshot { verified }),
            Arc::new(Seams { known }),
            limits,
        ),
        store,
    }
}

fn node() -> NodeState {
    NodeState {
        node_id: NODE.to_owned(),
        tenant: TENANT.to_owned(),
        generation: 0,
        applied_position: None,
    }
}

const HUB: HubHead = HubHead {
    generation: 1,
    head_position: POSITION + 20,
};

fn applier(store: Arc<dyn ProgressStore>, target: Arc<Target>) -> ChunkApplier {
    ChunkApplier::new("hub", store, target)
}

/// Serves and applies every chunk from `from`, acknowledging each; returns the last result.
fn run(rig: &Rig, applier: &ChunkApplier, from: u64) -> Applied {
    let mut index = from;
    loop {
        let wire = rig.server.serve_bootstrap(&node(), HUB, index).unwrap();
        let applied = applier.apply(&wire).unwrap();
        rig.server.ack(NODE, index, 0).unwrap();
        if let Applied::Complete { .. } = applied {
            return applied;
        }
        index += 1;
    }
}

#[test]
fn every_record_arrives_once_and_the_last_chunk_gives_the_live_position() {
    let rig = rig(true, true);
    let target = Arc::new(Target::default());
    let node_store = Arc::new(MemoryProgress::default());
    let applied = run(&rig, &applier(node_store, target.clone()), 0);
    assert_eq!(
        applied,
        Applied::Complete {
            resume_from: Some(POSITION + 1)
        }
    );
    let mut got = target.applied.lock().unwrap().clone();
    got.sort();
    let want: Vec<String> = (0..7).map(|n| format!("item-{n:04}")).collect();
    assert_eq!(got, want);
    // The hub's bookkeeping ended with the bootstrap.
    assert!(rig.store.load(NODE).unwrap().is_none());
}

#[test]
fn a_cut_resumes_at_the_checkpoint_and_repeats_nothing() {
    let rig = rig(true, true);
    let target = Arc::new(Target::default());
    let node_store: Arc<dyn ProgressStore> = Arc::new(MemoryProgress::default());
    let first = applier(Arc::clone(&node_store), target.clone());
    for index in 0..2 {
        let wire = rig.server.serve_bootstrap(&node(), HUB, index).unwrap();
        assert_eq!(first.apply(&wire).unwrap(), Applied::Next);
        rig.server.ack(NODE, index, 0).unwrap();
    }
    // The node restarts: a new applier over the same checkpoint asks for chunk 2.
    let second = applier(Arc::clone(&node_store), target.clone());
    assert_eq!(second.next_index().unwrap(), 2);
    let applied = run(&rig, &second, 2);
    assert!(matches!(applied, Applied::Complete { .. }));
    let got = target.applied.lock().unwrap().clone();
    assert_eq!(got.len(), 7, "no record applied twice");
    // A chunk delivered again after the cut is a duplicate and changes nothing.
    let again = rig.server.serve_bootstrap(&node(), HUB, 0).unwrap();
    let fresh = applier(
        Arc::new(MemoryProgress::default()),
        Arc::new(Target::default()),
    );
    assert!(fresh.apply(&again).is_ok());
}

#[test]
fn a_tampered_chunk_is_refused_whole_and_nothing_is_applied() {
    let rig = rig(true, true);
    let target = Arc::new(Target::default());
    let node_store = Arc::new(MemoryProgress::default());
    let applier = applier(node_store.clone(), target.clone());
    let mut wire = rig.server.serve_bootstrap(&node(), HUB, 0).unwrap();
    wire.body = wire.body.replace("Item 0", "Item X");
    assert_eq!(applier.apply(&wire), Err(ServeError::Tampered));
    assert!(target.applied.lock().unwrap().is_empty());
    assert_eq!(applier.next_index().unwrap(), 0);
}

#[test]
fn an_out_of_order_chunk_is_refused() {
    let rig = rig(true, true);
    let target = Arc::new(Target::default());
    let applier = applier(Arc::new(MemoryProgress::default()), target.clone());
    let wire = rig.server.serve_bootstrap(&node(), HUB, 1).unwrap();
    assert_eq!(applier.apply(&wire), Err(ServeError::OutOfOrder));
    assert!(target.applied.lock().unwrap().is_empty());
    // The hub refuses an acknowledgement out of order too.
    assert!(rig.server.ack(NODE, 3, 0).is_err());
}

#[test]
fn a_failed_apply_does_not_move_the_checkpoint() {
    let rig = rig(true, true);
    let target = Arc::new(Target {
        fail: true,
        ..Target::default()
    });
    let applier = applier(Arc::new(MemoryProgress::default()), target);
    let wire = rig.server.serve_bootstrap(&node(), HUB, 0).unwrap();
    assert_eq!(
        applier.apply(&wire),
        Err(ServeError::ApplyFailed("apply.refused"))
    );
    assert_eq!(applier.next_index().unwrap(), 0);
}

#[test]
fn an_unknown_scope_or_an_unverified_snapshot_serves_nothing() {
    let unknown = rig(false, true);
    assert_eq!(
        unknown.server.serve_bootstrap(&node(), HUB, 0).unwrap_err(),
        ServeError::ScopeUnknown
    );
    let unverified = rig(true, false);
    assert_eq!(
        unverified
            .server
            .serve_bootstrap(&node(), HUB, 0)
            .unwrap_err(),
        ServeError::NoVerifiedSnapshot
    );
}

#[test]
fn a_chunk_beyond_the_plan_is_out_of_range() {
    let rig = rig(true, true);
    assert_eq!(
        rig.server.serve_bootstrap(&node(), HUB, 99).unwrap_err(),
        ServeError::OutOfRange
    );
}

#[test]
fn a_catch_up_plan_is_served_piece_by_piece_and_applied_in_order() {
    let rig = rig(true, true);
    let mut mandatory = Event::new(41, "sys_sync_missing", Payload::new("{}"));
    mandatory.mandatory = true;
    let audit = Event::new(43, "sys_doc_audit", Payload::new("{}"));
    let plan = CatchUpPlan {
        mode: Mode::Coalesced,
        reasons: Vec::new(),
        mandatory_lane: vec![mandatory],
        bulk: Vec::new(),
        audit: vec![audit],
    };
    let target = Arc::new(Target::default());
    let applier = applier(Arc::new(MemoryProgress::default()), target.clone());
    let first = rig.server.serve_catch_up(NODE, &plan, 0).unwrap();
    let second = rig.server.serve_catch_up(NODE, &plan, 1).unwrap();
    assert!(!first.last);
    assert!(second.last);
    assert_eq!(applier.apply(&first).unwrap(), Applied::Next);
    assert_eq!(
        applier.apply(&second).unwrap(),
        Applied::Complete { resume_from: None }
    );
    assert_eq!(*target.events.lock().unwrap(), vec![41, 43]);
    assert_eq!(
        rig.server.serve_catch_up(NODE, &plan, 2).unwrap_err(),
        ServeError::OutOfRange
    );
}
