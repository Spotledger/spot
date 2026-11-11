//! Tests of `check_against`: each seeded defect yields exactly the finding it should.

use crate::{Finding, SegmentSummary, append_segments, check_against};

use super::{hash, journal, manifest, tiling, verified};

/// The findings of `observed` against a generation-1 manifest of two segments (1..=5, 6..=10).
fn findings(observed: &[SegmentSummary]) -> Vec<Finding> {
    check_against(&verified(&journal(10, 5)), observed)
}

/// The findings of `observed` against a generation-1 manifest that lists no segment yet.
fn findings_against_nothing(observed: &[SegmentSummary]) -> Vec<Finding> {
    check_against(&verified(&journal(0, 5)), observed)
}

#[test]
fn a_clean_journal_yields_no_findings() {
    let manifest = journal(10, 5);
    let verified = verified(&manifest);
    assert!(check_against(&verified, &manifest.segments).is_empty());
    assert!(verified.check_against(&manifest.segments).is_empty());
    assert!(
        !check_against(&verified, &[]).is_empty(),
        "an empty storage misses both segments"
    );
}

#[test]
fn a_manifest_without_segments_against_empty_storage_is_clean() {
    let manifest = journal(0, 5);
    assert!(manifest.segments.is_empty());
    assert!(check_against(&verified(&manifest), &[]).is_empty());
}

#[test]
fn a_missing_segment_is_reported_with_the_range_the_manifest_lists() {
    let manifest = journal(10, 5);
    let observed = vec![manifest.segments[0]];
    assert_eq!(
        findings(&observed),
        vec![Finding::MissingSegment {
            generation: 1,
            first_seq: 6,
            last_seq: 10,
        }]
    );
    assert!(findings(&observed)[0].is_tampering());
    assert_eq!(findings(&observed)[0].kind(), "missing_segment");
    assert_eq!(findings(&observed)[0].range(), (6, 10));
    assert_eq!(findings(&observed)[0].generation(), 1);
}

#[test]
fn a_segment_storage_holds_and_the_manifest_does_not_is_reported_but_is_not_tampering() {
    let manifest = journal(10, 5);
    // The whole journal, plus the same first segment twice: a crash between sealing and the
    // manifest update, or a repeated copy.
    let observed = vec![
        manifest.segments[0],
        manifest.segments[1],
        manifest.segments[0],
    ];
    let reported = findings(&observed);
    assert_eq!(
        reported,
        vec![Finding::UnlistedSegment {
            generation: 1,
            first_seq: 1,
            last_seq: 5,
        }]
    );
    assert!(
        !reported[0].is_tampering(),
        "an extra segment is not tampering"
    );
    assert_eq!(reported[0].kind(), "unlisted_segment");
}

#[test]
fn a_forged_segment_is_a_hash_mismatch() {
    let manifest = journal(10, 5);
    let forged = SegmentSummary::new(1, 6, 10, 5, hash(0xfe), hash(0x11));
    let observed = vec![manifest.segments[0], forged];
    let reported = findings(&observed);
    assert_eq!(
        reported,
        vec![Finding::HashMismatch {
            generation: 1,
            first_seq: 6,
            last_seq: 10,
        }]
    );
    assert!(reported[0].is_tampering());
    assert_eq!(reported[0].kind(), "hash_mismatch");
}

#[test]
fn a_forged_chain_link_is_a_hash_mismatch() {
    let manifest = journal(10, 5);
    let relinked = SegmentSummary::new(1, 6, 10, 5, manifest.segments[1].segment_hash, hash(0x77));
    assert_eq!(
        findings(&[manifest.segments[0], relinked]),
        vec![Finding::HashMismatch {
            generation: 1,
            first_seq: 6,
            last_seq: 10,
        }]
    );
}

#[test]
fn a_swapped_pair_is_two_hash_mismatches() {
    // The segments keep their position ranges but exchange their payloads: each listed range then
    // holds the other segment's hash.
    let manifest = journal(10, 5);
    let swapped = vec![
        SegmentSummary::new(
            1,
            1,
            5,
            5,
            manifest.segments[1].segment_hash,
            manifest.segments[0].previous_hash,
        ),
        SegmentSummary::new(
            1,
            6,
            10,
            5,
            manifest.segments[0].segment_hash,
            manifest.segments[1].previous_hash,
        ),
    ];
    let reported = findings(&swapped);
    assert_eq!(reported.len(), 2);
    assert!(
        reported
            .iter()
            .all(|finding| matches!(finding, Finding::HashMismatch { generation: 1, .. }))
    );
    assert_eq!(reported[0].range(), (1, 5));
    assert_eq!(reported[1].range(), (6, 10));
}

#[test]
fn a_segment_moved_to_another_range_is_reordered() {
    // The manifest's second segment, now claiming 6..=9: no listed range matches, but its hash is
    // a listed segment's, so it sits at the wrong place.
    let manifest = journal(10, 5);
    let moved = SegmentSummary::new(
        1,
        6,
        9,
        4,
        manifest.segments[1].segment_hash,
        manifest.segments[1].previous_hash,
    );
    let reported = findings(&[manifest.segments[0], moved]);
    assert!(
        reported.contains(&Finding::Reordered {
            generation: 1,
            first_seq: 6,
            last_seq: 9,
        }),
        "the moved segment is named: {reported:?}"
    );
    assert!(
        reported.contains(&Finding::MissingSegment {
            generation: 1,
            first_seq: 6,
            last_seq: 10,
        }),
        "the range it left is missing: {reported:?}"
    );
    assert!(
        reported
            .iter()
            .find(|finding| matches!(finding, Finding::Reordered { .. }))
            .expect("a reordered finding")
            .is_tampering()
    );
}

#[test]
fn a_hole_after_the_manifest_is_a_position_gap() {
    let manifest = journal(10, 5);
    let later = SegmentSummary::new(1, 16, 20, 5, hash(0x90), hash(0x91));
    let reported = findings(&[manifest.segments[0], later]);
    assert_eq!(
        reported,
        vec![
            Finding::MissingSegment {
                generation: 1,
                first_seq: 6,
                last_seq: 10,
            },
            Finding::PositionGap {
                generation: 1,
                first_seq: 11,
                last_seq: 15,
            },
            Finding::UnlistedSegment {
                generation: 1,
                first_seq: 16,
                last_seq: 20,
            },
        ]
    );
    assert!(
        !reported[1].is_tampering(),
        "a hole is data that never reached a manifest"
    );
    assert_eq!(reported[1].kind(), "position_gap");
}

#[test]
fn a_hole_before_a_fresh_generations_first_segment_is_a_position_gap() {
    let later = SegmentSummary::new(1, 5, 9, 5, hash(0x90), hash(0x91));
    assert_eq!(
        findings_against_nothing(&[later]),
        vec![
            Finding::PositionGap {
                generation: 1,
                first_seq: 1,
                last_seq: 4,
            },
            Finding::UnlistedSegment {
                generation: 1,
                first_seq: 5,
                last_seq: 9,
            },
        ]
    );
}

#[test]
fn a_newer_generation_without_a_fork_is_fork_not_recorded() {
    let manifest = journal(10, 5);
    let next = SegmentSummary::new(2, 1, 5, 5, hash(0x90), hash(0x91));
    let observed = vec![manifest.segments[0], manifest.segments[1], next];
    let reported = findings(&observed);
    assert_eq!(
        reported,
        vec![Finding::ForkNotRecorded {
            generation: 2,
            first_seq: 1,
            last_seq: 5,
        }]
    );
    assert!(reported[0].is_tampering());
    assert_eq!(reported[0].kind(), "fork_not_recorded");
}

#[test]
fn positions_past_a_recorded_fork_point_are_fork_not_recorded() {
    let first = journal(10, 5);
    let second = append_segments(
        &first
            .fork(10, "rebuild to before the bad import", 0, 0)
            .expect("fork"),
        &tiling(2, 4, 4, 9),
    )
    .expect("append");
    let verified = verified(&second);
    let past = SegmentSummary::new(1, 11, 14, 4, hash(0x90), hash(0x91));
    let observed = vec![
        first.segments[0],
        first.segments[1],
        past,
        second.segments[0],
    ];
    assert_eq!(
        verified.check_against(&observed),
        vec![Finding::ForkNotRecorded {
            generation: 1,
            first_seq: 11,
            last_seq: 14,
        }]
    );
}

#[test]
fn the_abandoned_history_within_the_fork_point_is_accounted_for() {
    let first = journal(10, 5);
    let second = append_segments(
        &first.fork(10, "rebuild", 0, 0).expect("fork"),
        &tiling(2, 4, 4, 9),
    )
    .expect("append");
    let observed = vec![first.segments[0], first.segments[1], second.segments[0]];
    assert!(verified(&second).check_against(&observed).is_empty());
}

#[test]
fn a_store_that_has_the_manifest_and_one_extra_generation_reports_only_that() {
    // A generation-2 manifest whose storage still holds the abandoned generation: both the fork
    // record and the old segments are consistent, so nothing is reported.
    let first = journal(10, 5);
    let second = first.fork(10, "rebuild", 0, 0).expect("fork");
    let verified = verified(&second);
    assert!(verified.check_against(&first.segments).is_empty());
    assert_eq!(verified.forks().len(), 1);
}

#[test]
fn findings_are_sorted_by_position_and_carry_no_duplicates() {
    let manifest = journal(20, 5);
    let verified = verified(&manifest);
    let mut observed = manifest.segments.clone();
    observed.remove(3); // 16..=20 missing
    observed.remove(0); // 1..=5 missing
    let hunks = vec![
        Finding::MissingSegment {
            generation: 1,
            first_seq: 1,
            last_seq: 5,
        },
        Finding::MissingSegment {
            generation: 1,
            first_seq: 16,
            last_seq: 20,
        },
    ];
    assert_eq!(check_against(&verified, &observed), hunks);
    // Sorting is stable whoever orders the input: the same findings, the same order.
    let mut shuffled = observed.clone();
    shuffled.reverse();
    assert_eq!(check_against(&verified, &shuffled), hunks);
}

#[test]
fn a_finding_names_its_kind_and_its_range() {
    let kinds: Vec<&str> = [
        Finding::MissingSegment {
            generation: 3,
            first_seq: 1,
            last_seq: 2,
        },
        Finding::UnlistedSegment {
            generation: 3,
            first_seq: 3,
            last_seq: 4,
        },
        Finding::HashMismatch {
            generation: 3,
            first_seq: 5,
            last_seq: 6,
        },
        Finding::PositionGap {
            generation: 3,
            first_seq: 7,
            last_seq: 8,
        },
        Finding::Reordered {
            generation: 3,
            first_seq: 9,
            last_seq: 10,
        },
        Finding::ForkNotRecorded {
            generation: 4,
            first_seq: 11,
            last_seq: 12,
        },
    ]
    .iter()
    .map(Finding::kind)
    .collect();
    assert_eq!(
        kinds,
        vec![
            "missing_segment",
            "unlisted_segment",
            "hash_mismatch",
            "position_gap",
            "reordered",
            "fork_not_recorded"
        ]
    );
    let mut sorted = [
        Finding::ForkNotRecorded {
            generation: 4,
            first_seq: 11,
            last_seq: 12,
        },
        Finding::HashMismatch {
            generation: 3,
            first_seq: 5,
            last_seq: 6,
        },
        Finding::MissingSegment {
            generation: 3,
            first_seq: 1,
            last_seq: 2,
        },
    ];
    sorted.sort();
    assert_eq!(sorted[0].range(), (1, 2), "generation, then position");
    assert_eq!(sorted[2].generation(), 4);
}

#[test]
fn a_manifest_and_a_storage_of_another_source_are_compared_position_by_position() {
    // The source id is not part of the segment summary, so two sources that happen to hold the
    // same summaries compare clean: the caller scopes the comparison, not this function.
    let other = manifest("site:other", 1, tiling(1, 10, 5, 3));
    assert!(check_against(&verified(&other), &journal(10, 5).segments).is_empty());
}
