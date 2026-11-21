#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The mode table of ADR-066 section 2, one test per row, plus its boundaries and refusals.

mod common;

use spot_sync_catchup::{
    CatchUpError, ClassCounts, GapSummary, Mode, ModeReason, Settings, Share, choose_mode,
};

/// A gap that is small and still inside hot retention: the incremental row.
fn small_gap() -> GapSummary {
    GapSummary {
        applied_prefix: 100,
        stream_end: 200,
        hot_retention_floor: 50,
        in_scope_event_count: 20,
        class_counts: ClassCounts {
            state: 12,
            domain: 6,
            audit: 2,
        },
        estimated_coalesced_records: 12,
        estimated_coalesced_bytes: 4_000,
        estimated_snapshot_bytes: 100_000,
        node_is_new_or_resync: false,
    }
}

#[test]
fn a_new_or_resynced_node_is_snapshotted() {
    let summary = GapSummary {
        node_is_new_or_resync: true,
        ..small_gap()
    };
    let decision = choose_mode(&summary, &Settings::default()).unwrap();
    assert_eq!(decision.mode, Mode::Snapshot);
    assert_eq!(decision.reasons, vec![ModeReason::NewOrResync]);
}

#[test]
fn a_rebuilt_node_that_reported_resync_is_snapshotted_even_with_a_tiny_gap() {
    let summary = GapSummary {
        node_is_new_or_resync: true,
        in_scope_event_count: 0,
        class_counts: ClassCounts::default(),
        ..small_gap()
    };
    let decision = choose_mode(&summary, &Settings::default()).unwrap();
    assert_eq!(decision.mode, Mode::Snapshot);
    assert_eq!(decision.reasons, vec![ModeReason::NewOrResync]);
}

#[test]
fn a_small_gap_inside_hot_retention_is_incremental() {
    let decision = choose_mode(&small_gap(), &Settings::default()).unwrap();
    assert_eq!(decision.mode, Mode::Incremental);
    assert_eq!(
        decision.reasons,
        vec![ModeReason::SmallGap {
            in_scope_event_count: 20,
            incremental_limit: 10_000,
        }]
    );
}

#[test]
fn a_gap_over_the_limit_is_coalesced() {
    let summary = GapSummary {
        in_scope_event_count: 10_001,
        class_counts: ClassCounts {
            state: 9_000,
            domain: 1_001,
            audit: 0,
        },
        ..small_gap()
    };
    let decision = choose_mode(&summary, &Settings::default()).unwrap();
    assert_eq!(decision.mode, Mode::Coalesced);
    assert_eq!(
        decision.reasons,
        vec![ModeReason::GapOverLimit {
            in_scope_event_count: 10_001,
            incremental_limit: 10_000,
        }]
    );
}

#[test]
fn a_prefix_older_than_hot_retention_is_coalesced() {
    let summary = GapSummary {
        applied_prefix: 40,
        hot_retention_floor: 50,
        ..small_gap()
    };
    let decision = choose_mode(&summary, &Settings::default()).unwrap();
    assert_eq!(decision.mode, Mode::Coalesced);
    assert_eq!(
        decision.reasons,
        vec![ModeReason::PrefixOutsideHotRetention {
            applied_prefix: 40,
            hot_retention_floor: 50,
        }]
    );
}

#[test]
fn a_coalesced_plan_above_the_snapshot_share_is_snapshotted() {
    let summary = GapSummary {
        in_scope_event_count: 10_001,
        class_counts: ClassCounts {
            state: 10_001,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_bytes: 50_000,
        estimated_snapshot_bytes: 100_000,
        ..small_gap()
    };
    let decision = choose_mode(&summary, &Settings::default()).unwrap();
    assert_eq!(decision.mode, Mode::Snapshot);
    assert_eq!(decision.reasons.len(), 2);
    assert_eq!(
        decision.reasons[1],
        ModeReason::CoalescedCostAboveSnapshotShare {
            coalesced_bytes: 50_000,
            snapshot_bytes: 100_000,
            snapshot_fraction_basis_points: 4_000,
        }
    );
}

#[test]
fn a_coalesced_plan_at_exactly_the_share_is_still_coalesced() {
    let summary = GapSummary {
        in_scope_event_count: 10_001,
        class_counts: ClassCounts {
            state: 10_001,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_bytes: 40_000,
        estimated_snapshot_bytes: 100_000,
        ..small_gap()
    };
    let decision = choose_mode(&summary, &Settings::default()).unwrap();
    assert_eq!(decision.mode, Mode::Coalesced);
}

#[test]
fn a_gap_exactly_at_the_limit_is_incremental() {
    let summary = GapSummary {
        in_scope_event_count: 10_000,
        class_counts: ClassCounts {
            state: 9_000,
            domain: 1_000,
            audit: 0,
        },
        ..small_gap()
    };
    let decision = choose_mode(&summary, &Settings::default()).unwrap();
    assert_eq!(decision.mode, Mode::Incremental);
}

#[test]
fn the_share_and_the_limits_are_the_caller_s() {
    let summary = GapSummary {
        in_scope_event_count: 20,
        estimated_coalesced_bytes: 1_000,
        estimated_snapshot_bytes: 10_000,
        ..small_gap()
    };
    let strict = Settings {
        incremental_limit: 10,
        snapshot_fraction: Share::from_percent(5).unwrap(),
        ..Settings::default()
    };
    let decision = choose_mode(&summary, &strict).unwrap();
    assert_eq!(decision.mode, Mode::Snapshot);
}

#[test]
fn unusable_settings_are_refused() {
    let no_limit = Settings {
        incremental_limit: 0,
        ..Settings::default()
    };
    assert_eq!(
        choose_mode(&small_gap(), &no_limit).unwrap_err().code(),
        "sync_catchup.invalid_settings"
    );
    let no_bundles = Settings {
        bundle_records: 0,
        ..Settings::default()
    };
    assert_eq!(
        choose_mode(&small_gap(), &no_bundles).unwrap_err().code(),
        "sync_catchup.invalid_settings"
    );
    assert_eq!(
        Share::from_basis_points(10_001).unwrap_err(),
        CatchUpError::InvalidSettings {
            reason: "a snapshot_fraction above 100% is not a share of a whole",
        }
    );
}

#[test]
fn an_impossible_summary_is_refused() {
    let beyond_end = GapSummary {
        applied_prefix: 201,
        ..small_gap()
    };
    assert_eq!(
        choose_mode(&beyond_end, &Settings::default())
            .unwrap_err()
            .code(),
        "sync_catchup.inconsistent_summary"
    );
    let floor_beyond_end = GapSummary {
        hot_retention_floor: 201,
        ..small_gap()
    };
    assert_eq!(
        choose_mode(&floor_beyond_end, &Settings::default())
            .unwrap_err()
            .code(),
        "sync_catchup.inconsistent_summary"
    );
    let too_many_classes = GapSummary {
        in_scope_event_count: 19,
        ..small_gap()
    };
    assert_eq!(
        choose_mode(&too_many_classes, &Settings::default())
            .unwrap_err()
            .code(),
        "sync_catchup.inconsistent_summary"
    );
}
