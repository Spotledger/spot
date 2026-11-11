//! Tests of the append-only growth rule and of fork records.

use crate::{Fork, Manifest, ManifestError, SegmentSummary, append_segments};

use super::{hash, journal, manifest, tiling};

#[test]
fn appending_contiguous_segments_grows_the_manifest() {
    let first = Manifest::first("site:demo", 7).expect("a valid first manifest");
    assert_eq!(first.last_seq(), 0);
    assert_eq!(first.next_seq(), 1);
    let grown = append_segments(&first, &tiling(1, 10, 5, 3)).expect("contiguous segments append");
    assert_eq!(grown.segments.len(), 2);
    assert_eq!(grown.last_seq(), 10);
    assert_eq!(grown.next_seq(), 11);
    assert_eq!(grown.generation, 1);
    assert_eq!(grown.source, "site:demo");
    assert_eq!(grown.issued_at_millis, 7, "appending is not issuing");
    grown.validate().expect("the grown manifest is valid");
}

#[test]
fn appending_keeps_every_earlier_summary_exactly() {
    let first = journal(10, 5);
    let grown =
        append_segments(&first, &tiling(1, 20, 5, 3)[2..]).expect("the next segments append");
    assert_eq!(grown.segments[..2], first.segments[..]);
    assert_eq!(grown.segments.len(), 4);
    assert_eq!(
        first.segments.len(),
        2,
        "the previous manifest is untouched"
    );
}

#[test]
fn appending_nothing_returns_the_manifest_unchanged() {
    let first = journal(10, 5);
    assert_eq!(
        append_segments(&first, &[]).expect("nothing appends"),
        first
    );
}

#[test]
fn a_gap_is_refused() {
    let first = journal(10, 5);
    let late = SegmentSummary::new(1, 12, 15, 4, hash(0x70), hash(0x71));
    let error = append_segments(&first, &[late]).expect_err("a gap is refused");
    assert_eq!(
        error,
        ManifestError::AppendGap {
            expected: 11,
            offered: 12
        }
    );
    assert_eq!(error.code(), "journal_manifest.append_gap");
}

#[test]
fn a_gap_between_the_offered_segments_is_refused() {
    let first = Manifest::first("site:demo", 0).expect("a valid first manifest");
    let offered = vec![
        SegmentSummary::new(1, 1, 5, 5, hash(0x70), hash(0x71)),
        SegmentSummary::new(1, 8, 10, 3, hash(0x72), hash(0x70)),
    ];
    assert_eq!(
        append_segments(&first, &offered).expect_err("a gap is refused"),
        ManifestError::AppendGap {
            expected: 6,
            offered: 8
        }
    );
}

#[test]
fn an_overlap_is_refused() {
    let first = journal(10, 5);
    let repeated = SegmentSummary::new(1, 8, 12, 5, hash(0x70), hash(0x71));
    let error = append_segments(&first, &[repeated]).expect_err("an overlap is refused");
    assert_eq!(
        error,
        ManifestError::AppendOverlap {
            expected: 11,
            offered: 8
        }
    );
    assert_eq!(error.code(), "journal_manifest.append_overlap");
}

#[test]
fn an_appended_manifest_that_rewrites_history_is_refused() {
    let first = journal(10, 5);
    // The same positions, re-described with another hash: an appended summary that changes an
    // earlier one.
    let rewritten = SegmentSummary::new(1, 1, 5, 5, hash(0x99), hash(0x98));
    assert_eq!(
        append_segments(&first, &[rewritten]).expect_err("an earlier summary cannot change"),
        ManifestError::AppendOverlap {
            expected: 11,
            offered: 1
        }
    );
}

#[test]
fn a_new_segment_that_does_not_link_to_the_last_one_is_refused() {
    let first = journal(10, 5);
    let unlinked = SegmentSummary::new(1, 11, 15, 5, hash(0x70), hash(0x99));
    let error = append_segments(&first, &[unlinked]).expect_err("a broken link is refused");
    assert_eq!(error, ManifestError::HistoryRewritten { first_seq: 11 });
    assert_eq!(error.code(), "journal_manifest.history_rewritten");
}

#[test]
fn a_generation_change_without_a_fork_is_refused() {
    let first = journal(10, 5);
    let next_generation = tiling(2, 5, 5, 9);
    let error = append_segments(&first, &next_generation).expect_err("a generation needs a fork");
    assert_eq!(
        error,
        ManifestError::GenerationChange {
            expected: 1,
            offered: 2
        }
    );
    assert_eq!(error.code(), "journal_manifest.generation_change");
}

#[test]
fn a_fork_opens_the_next_generation_and_keeps_the_history() {
    let first = journal(10, 5);
    let second = first
        .fork(
            10,
            "point-in-time rebuild to before the bad import",
            900,
            901,
        )
        .expect("the fork is valid");
    assert_eq!(second.generation, 2);
    assert!(
        second.segments.is_empty(),
        "the new generation starts empty"
    );
    assert_eq!(second.forks.len(), 1);
    assert_eq!(second.forks[0].generation, 2);
    assert_eq!(second.forks[0].forked_from_position, 10);
    assert_eq!(second.previous_manifest_hash, Some(first.manifest_hash()));
    assert_eq!(second.issued_at_millis, 901);
    second.validate().expect("the forked manifest is valid");

    // The new generation grows from position 1, and its first segment needs no link to the
    // abandoned one.
    let grown = append_segments(&second, &tiling(2, 4, 4, 9)).expect("the new generation appends");
    assert_eq!(grown.generation, 2);
    assert_eq!(grown.segments[0].first_seq, 1);
    assert_eq!(grown.generation, 2);
    grown.validate().expect("the grown manifest is valid");
}

#[test]
fn a_fork_at_a_position_the_generation_lacks_is_refused() {
    let first = journal(10, 5);
    assert!(matches!(
        first.fork(11, "beyond the end", 0, 0),
        Err(ManifestError::ForkInvalid(_))
    ));
    assert!(matches!(
        first.fork(0, "no position", 0, 0),
        Err(ManifestError::ForkInvalid(_))
    ));
    let empty = Manifest::first("site:demo", 0).expect("a valid first manifest");
    assert!(matches!(
        empty.fork(1, "nothing to fork", 0, 0),
        Err(ManifestError::ForkInvalid(_))
    ));
}

#[test]
fn a_fork_record_is_accepted_only_with_a_new_generation() {
    // A fork opens a generation after the first: generation 1 and 0 are not later generations.
    assert!(matches!(
        Fork::new(1, 5, "same generation", 0).validate(),
        Err(ManifestError::ForkInvalid(_))
    ));
    assert!(matches!(
        Fork::new(0, 5, "generation zero", 0).validate(),
        Err(ManifestError::ForkInvalid(_))
    ));
    assert!(Fork::new(2, 5, "a later generation", 0).validate().is_ok());

    // A generation-1 manifest cannot carry a fork record at all.
    let mut first = journal(10, 5);
    first.forks = vec![Fork::new(2, 10, "not opened yet", 0)];
    assert!(matches!(
        first.validate(),
        Err(ManifestError::ForkInvalid(_))
    ));

    // A generation-2 manifest without its fork record does not reach its own generation.
    let mut second = manifest("site:demo", 2, tiling(2, 4, 4, 9));
    second.forks.clear();
    assert!(matches!(
        second.validate(),
        Err(ManifestError::ForkInvalid(_))
    ));

    // A fork history that skips a generation is refused.
    let mut skipping = manifest("site:demo", 3, tiling(3, 4, 4, 9));
    skipping.forks = vec![Fork::new(3, 10, "skips generation 2", 0)];
    assert!(matches!(
        skipping.validate(),
        Err(ManifestError::ForkInvalid(_))
    ));
    // And so is a repeated generation.
    skipping.forks = vec![
        Fork::new(2, 10, "first", 0),
        Fork::new(2, 10, "again", 0),
        Fork::new(3, 10, "third", 0),
    ];
    assert!(matches!(
        skipping.validate(),
        Err(ManifestError::ForkInvalid(_))
    ));
}

#[test]
fn a_fork_record_needs_a_position_and_a_reason() {
    assert!(matches!(
        Fork::new(2, 0, "at position zero", 0).validate(),
        Err(ManifestError::ForkInvalid(_))
    ));
    assert!(matches!(
        Fork::new(2, 4, "   ", 0).validate(),
        Err(ManifestError::ForkInvalid(_))
    ));
    assert!(matches!(
        Fork::new(2, 4, "", 0).validate(),
        Err(ManifestError::ForkInvalid(_))
    ));
    assert_eq!(
        Fork::new(2, 0, "x", 0)
            .validate()
            .expect_err("a fork point is a position")
            .code(),
        "journal_manifest.fork_invalid"
    );
}

#[test]
fn a_manifest_with_a_hole_in_its_own_segments_is_refused() {
    let mut holed = journal(10, 5);
    holed.segments.remove(0);
    let error = holed.validate().expect_err("a hole is refused");
    assert_eq!(
        error,
        ManifestError::ManifestGap {
            first_seq: 1,
            last_seq: 5
        }
    );
    assert_eq!(error.code(), "journal_manifest.manifest_gap");

    let mut gapped = journal(15, 5);
    gapped.segments.remove(1);
    assert!(matches!(
        gapped.validate(),
        Err(ManifestError::ManifestGap { .. })
    ));
}

#[test]
fn a_manifest_that_lists_a_range_twice_is_refused() {
    let mut twice = journal(10, 5);
    twice.segments[1] = twice.segments[0];
    assert!(matches!(
        twice.validate(),
        Err(ManifestError::DuplicateSegment { .. })
    ));
    assert_eq!(
        twice
            .validate()
            .expect_err("a repeated range is refused")
            .code(),
        "journal_manifest.duplicate_segment"
    );
}

#[test]
fn a_manifest_whose_chain_link_is_broken_is_refused() {
    let mut broken = journal(10, 5);
    broken.segments[1].previous_hash = hash(0x77);
    assert_eq!(
        broken.validate().expect_err("a broken link is refused"),
        ManifestError::HistoryRewritten { first_seq: 6 }
    );
}

#[test]
fn a_manifest_with_a_segment_of_another_generation_is_refused() {
    let mut mixed = journal(10, 5);
    mixed.segments[1].generation = 2;
    assert_eq!(
        mixed.validate().expect_err("a mixed generation is refused"),
        ManifestError::SegmentGeneration {
            expected: 1,
            offered: 2
        }
    );
}

#[test]
fn a_manifest_without_a_previous_link_after_a_fork_is_refused() {
    let mut unlinked = manifest("site:demo", 2, tiling(2, 4, 4, 9));
    unlinked.previous_manifest_hash = None;
    assert_eq!(
        unlinked
            .validate()
            .expect_err("generation 2 needs a previous link"),
        ManifestError::PreviousManifestInvalid
    );
    let mut first = journal(4, 4);
    first.previous_manifest_hash = Some(hash(0x5a));
    assert_eq!(
        first
            .validate()
            .expect_err("generation 1 has no previous link"),
        ManifestError::PreviousManifestInvalid
    );
}

#[test]
fn a_manifest_needs_a_source_and_a_generation() {
    let mut no_source = journal(4, 4);
    no_source.source = "  ".to_owned();
    assert_eq!(
        no_source.validate().expect_err("a source is required"),
        ManifestError::SourceInvalid
    );
    let mut zero = journal(4, 4);
    zero.generation = 0;
    assert_eq!(
        zero.validate().expect_err("generations start at one"),
        ManifestError::GenerationInvalid { generation: 0 }
    );
    assert_eq!(
        ManifestError::SourceInvalid.code(),
        "journal_manifest.source_invalid"
    );
}

#[test]
fn a_segment_summary_must_fill_its_range() {
    let wrong_count = SegmentSummary::new(1, 6, 10, 4, hash(0x70), hash(0x71));
    assert_eq!(
        wrong_count
            .validate()
            .expect_err("the count fills the range"),
        ManifestError::SegmentRangeInvalid {
            first_seq: 6,
            last_seq: 10
        }
    );
    let reversed = SegmentSummary::new(1, 10, 6, 1, hash(0x70), hash(0x71));
    assert!(matches!(
        reversed.validate(),
        Err(ManifestError::SegmentRangeInvalid { .. })
    ));
    let zero_position = SegmentSummary::new(1, 0, 0, 1, hash(0x70), hash(0x71));
    assert!(matches!(
        zero_position.validate(),
        Err(ManifestError::SegmentRangeInvalid { .. })
    ));
    assert!(
        SegmentSummary::new(1, 6, 10, 5, hash(0x70), hash(0x71))
            .validate()
            .is_ok()
    );
}
