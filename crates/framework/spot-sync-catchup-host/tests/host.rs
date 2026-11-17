//! The director, the parallel-bootstrap bound, resumable progress and the skew gate, with virtual
//! time (every instant is an argument).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_sync_catchup::{ClassCounts, GapSummary, ModeReason};
use spot_sync_catchup_host::{
    CatchupConfig, ChunkAck, Director, HostError, MemoryProgress, ProgressStore, Route, SkewGate,
};
use spot_sync_skew::{Decision, NodeReport, SoftwareVersion};

fn summary(
    applied: u64,
    end: u64,
    floor: u64,
    events: u64,
    coalesced: u64,
    snapshot: u64,
    new: bool,
) -> GapSummary {
    GapSummary {
        applied_prefix: applied,
        stream_end: end,
        hot_retention_floor: floor,
        in_scope_event_count: events,
        class_counts: ClassCounts {
            state: events,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_records: events / 4,
        estimated_coalesced_bytes: coalesced,
        estimated_snapshot_bytes: snapshot,
        node_is_new_or_resync: new,
    }
}

fn director(config: &CatchupConfig, store: &Arc<MemoryProgress>) -> Director {
    Director::new(config, store.clone()).unwrap()
}

/// A gap a node two days behind has: 20,000 changes, the prefix still in the hot queue, a coalesced
/// plan well under the snapshot's cost.
fn two_days() -> GapSummary {
    summary(1_000, 21_000, 500, 20_000, 1_000_000, 100_000_000, false)
}

#[test]
fn a_small_gap_stays_live_a_large_one_is_coalesced_and_a_new_node_bootstraps() {
    let store = Arc::new(MemoryProgress::default());
    let d = director(&CatchupConfig::default(), &store);
    let small = summary(1_000, 1_100, 500, 100, 10_000, 100_000_000, false);
    assert_eq!(d.route("edge-1", &small, 0).unwrap(), Route::Live);
    assert!(
        store.load("edge-1").unwrap().is_none(),
        "live keeps no plan"
    );
    match d.route("edge-2", &two_days(), 0).unwrap() {
        Route::CatchUp {
            reasons,
            resume_from_chunk,
        } => {
            assert_eq!(resume_from_chunk, 0);
            assert!(
                reasons
                    .iter()
                    .any(|r| matches!(r, ModeReason::GapOverLimit { .. }))
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(d.status_text("edge-2").unwrap(), "catching_up (planning)");
    let new = summary(0, 5_000, 1, 5_000, 0, 50_000_000, true);
    assert_eq!(
        d.route("edge-3", &new, 0).unwrap(),
        Route::Bootstrap {
            resume_from_chunk: 0
        }
    );
}

#[test]
fn a_coalesced_plan_dearer_than_the_snapshot_share_becomes_a_bootstrap() {
    let store = Arc::new(MemoryProgress::default());
    let d = director(&CatchupConfig::default(), &store);
    // 60% of the snapshot's bytes: over the 40% share.
    let dear = summary(1_000, 21_000, 500, 20_000, 60_000_000, 100_000_000, false);
    assert_eq!(
        d.route("edge-1", &dear, 0).unwrap(),
        Route::Bootstrap {
            resume_from_chunk: 0
        }
    );
}

#[test]
fn twenty_nodes_returning_at_once_respect_the_parallel_bootstrap_bound() {
    let config = CatchupConfig {
        max_parallel_bootstraps: 4,
        ..CatchupConfig::default()
    };
    let store = Arc::new(MemoryProgress::default());
    let d = director(&config, &store);
    let new = summary(0, 5_000, 1, 5_000, 0, 50_000_000, true);
    let mut admitted = Vec::new();
    let mut waiting = Vec::new();
    for n in 0..20 {
        let node = format!("edge-{n}");
        match d.route(&node, &new, 0).unwrap() {
            Route::Bootstrap { .. } => admitted.push(node),
            Route::Wait { retry_after_ms } => {
                assert!(
                    retry_after_ms >= config.retry_base_ms,
                    "a base plus a spread"
                );
                waiting.push(node);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((admitted.len(), waiting.len()), (4, 16));
    // A waiting node holds no stored plan: it has not started.
    assert!(store.load(&waiting[0]).unwrap().is_none());
    // One finishes: a slot frees and the first waiting node is admitted.
    d.finish(&admitted[0]).unwrap();
    assert_eq!(
        d.route(&waiting[0], &new, 1_000).unwrap(),
        Route::Bootstrap {
            resume_from_chunk: 0
        }
    );
}

#[test]
fn a_bootstrap_resumes_at_the_next_chunk_after_a_restart_and_never_from_zero() {
    let store = Arc::new(MemoryProgress::default());
    let first = director(&CatchupConfig::default(), &store);
    let new = summary(0, 5_000, 1, 5_000, 0, 50_000_000, true);
    assert!(matches!(
        first.route("edge-1", &new, 0).unwrap(),
        Route::Bootstrap { .. }
    ));
    first.plan_cut("edge-1", 10).unwrap();
    for index in 0..4 {
        let ack = first
            .chunk_done("edge-1", index, i64::try_from(index).unwrap() * 1_000)
            .unwrap();
        assert!(matches!(ack, ChunkAck::Next | ChunkAck::Wait(_)));
    }
    assert_eq!(
        first.status_text("edge-1").unwrap(),
        "bootstrapping 40% (chunk 4 of 10)"
    );
    // The hub restarts: a new director over the same store.
    let second = director(&CatchupConfig::default(), &store);
    assert_eq!(
        second.route("edge-1", &new, 10_000).unwrap(),
        Route::Bootstrap {
            resume_from_chunk: 4
        }
    );
    // An out-of-order chunk is refused; the next one is accepted.
    assert_eq!(
        second.chunk_done("edge-1", 7, 10_000).unwrap_err(),
        HostError::ChunkOutOfOrder
    );
    assert!(second.chunk_done("edge-1", 4, 20_000).is_ok());
}

#[test]
fn the_last_chunk_completes_the_bootstrap_and_finish_frees_the_slot() {
    let store = Arc::new(MemoryProgress::default());
    let d = director(&CatchupConfig::default(), &store);
    let new = summary(0, 5_000, 1, 5_000, 0, 50_000_000, true);
    d.route("edge-1", &new, 0).unwrap();
    d.plan_cut("edge-1", 2).unwrap();
    assert!(matches!(
        d.chunk_done("edge-1", 0, 0).unwrap(),
        ChunkAck::Next | ChunkAck::Wait(_)
    ));
    assert_eq!(
        d.chunk_done("edge-1", 1, 10_000).unwrap(),
        ChunkAck::Complete
    );
    d.finish("edge-1").unwrap();
    assert_eq!(d.status_text("edge-1").unwrap(), "live");
    assert!(store.load("edge-1").unwrap().is_none());
}

#[test]
fn a_catch_up_in_progress_resumes_its_chunk_index() {
    let store = Arc::new(MemoryProgress::default());
    let d = director(&CatchupConfig::default(), &store);
    d.route("edge-1", &two_days(), 0).unwrap();
    d.plan_cut("edge-1", 8).unwrap();
    d.chunk_done("edge-1", 0, 0).unwrap();
    d.chunk_done("edge-1", 1, 0).unwrap();
    let restarted = director(&CatchupConfig::default(), &store);
    match restarted.route("edge-1", &two_days(), 5_000).unwrap() {
        Route::CatchUp {
            resume_from_chunk, ..
        } => assert_eq!(resume_from_chunk, 2),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_configuration_has_bounds_and_refuses_unknown_keys() {
    assert!(CatchupConfig::default().validate().is_ok());
    for bad in [
        CatchupConfig {
            max_parallel_bootstraps: 0,
            ..CatchupConfig::default()
        },
        CatchupConfig {
            snapshot_share_percent: 101,
            ..CatchupConfig::default()
        },
        CatchupConfig {
            catchup_threshold_events: 0,
            ..CatchupConfig::default()
        },
        CatchupConfig {
            chunk_burst: 0,
            ..CatchupConfig::default()
        },
    ] {
        assert_eq!(
            bad.validate().unwrap_err().code(),
            "sync_catchup_host.config_invalid"
        );
    }
    let unknown = serde_json::from_str::<CatchupConfig>(r#"{"catchup_threshold": 1}"#);
    assert!(unknown.is_err(), "unknown keys are refused");
}

fn report(major: u32, minor: u32, topics: &[(&str, &[u32])]) -> NodeReport {
    let mut decodable = BTreeMap::new();
    for (topic, majors) in topics {
        decodable.insert(
            (*topic).to_owned(),
            majors.iter().copied().collect::<BTreeSet<u32>>(),
        );
    }
    NodeReport {
        software_version: SoftwareVersion {
            major,
            minor,
            patch: 0,
        },
        metadata_version: 1,
        migration_epoch: 1,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: decodable,
    }
}

#[test]
fn a_node_one_release_behind_keeps_syncing_and_an_upgrade_releases_its_holds() {
    let hub = report(2, 4, &[("doc_state", &[1, 2])]);
    let gate = SkewGate::new(hub, 1);
    let behind = report(2, 3, &[("doc_state", &[1])]);
    let hello = gate.on_hello("edge-1", &behind);
    assert_eq!(
        hello.decision,
        Decision::Run,
        "one release behind still syncs"
    );
    assert!(!hello.upgraded, "the first hello is not an upgrade");
    assert!(
        gate.must_hold(&behind, "doc_state", 2),
        "a major it cannot decode is held"
    );
    assert!(!gate.must_hold(&behind, "doc_state", 1));
    // It reconnects with a higher capability: the holds are released.
    let upgraded = report(2, 4, &[("doc_state", &[1, 2])]);
    let hello = gate.on_hello("edge-1", &upgraded);
    assert!(hello.upgraded);
    assert_eq!(hello.decision, Decision::Run);
    assert!(!gate.must_hold(&upgraded, "doc_state", 2));
    // The same hello again is not an upgrade.
    assert!(!gate.on_hello("edge-1", &upgraded).upgraded);
}

#[test]
fn a_node_too_far_behind_is_refused_with_a_catalogue_code() {
    let gate = SkewGate::new(report(2, 4, &[]), 1);
    let hello = gate.on_hello("edge-9", &report(1, 0, &[]));
    assert!(matches!(hello.decision, Decision::Refuse { .. }));
}
