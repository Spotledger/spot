use super::build;
use crate::{
    ChainError, SegmentBytes, genesis_hash, segment_hash, verify_chain, verify_chain_from,
};

/// Five chained segments of 10 entries each: seqs 1..=50, small blocks so each has several.
fn five() -> Vec<crate::SealedSegment> {
    let mut v: Vec<crate::SealedSegment> = Vec::new();
    for i in 0..5u64 {
        let prev = v.last().map(|s| s.hash);
        v.push(build("tenant-a", 1, 1 + i * 10, 10, prev, 200));
    }
    v
}

fn bytes(v: &[&crate::SealedSegment]) -> Vec<SegmentBytes> {
    v.iter().map(|s| s.bytes.clone()).collect()
}

#[test]
fn an_intact_chain_verifies_and_reports_its_extent() {
    let s = five();
    let all: Vec<&crate::SealedSegment> = s.iter().collect();
    let r = verify_chain(&bytes(&all)).expect("chain");
    assert_eq!(r.segments, 5);
    assert_eq!(r.entries, 50);
    assert_eq!(r.first.map(|p| p.seq), Some(1));
    assert_eq!(r.last.map(|p| p.seq), Some(50));
    assert_eq!(r.head_hash, s[4].hash);
    assert_eq!(s[0].header.prev_hash, genesis_hash("tenant-a"));
    assert_eq!(s[1].header.prev_hash, segment_hash(&s[0].bytes));
}

#[test]
fn an_empty_list_is_an_empty_chain() {
    let none: Vec<SegmentBytes> = Vec::new();
    let r = verify_chain(&none).expect("empty");
    assert_eq!((r.segments, r.entries, r.first, r.last), (0, 0, None, None));
}

#[test]
fn a_dropped_segment_is_missing() {
    let s = five();
    let e = verify_chain(&bytes(&[&s[0], &s[1], &s[3], &s[4]])).unwrap_err();
    assert_eq!(e, ChainError::Missing { index: 2 });
    let e = verify_chain(&bytes(&[&s[1], &s[2]])).unwrap_err();
    assert_eq!(e, ChainError::Missing { index: 0 });
}

#[test]
fn a_duplicated_segment_overlaps() {
    let s = five();
    let e = verify_chain(&bytes(&[&s[0], &s[1], &s[1], &s[2]])).unwrap_err();
    assert_eq!(e, ChainError::PositionGap { index: 2 });
}

#[test]
fn swapped_segments_are_reordered() {
    let s = five();
    let e = verify_chain(&bytes(&[&s[0], &s[2], &s[1], &s[3], &s[4]])).unwrap_err();
    assert_eq!(e, ChainError::Reordered { index: 1 });
}

#[test]
fn an_altered_segment_is_named() {
    let s = five();
    for victim in 0..5 {
        let mut b = bytes(&s.iter().collect::<Vec<_>>());
        let mid = b[victim].len() / 2;
        b[victim][mid] ^= 0x01;
        match verify_chain(&b).unwrap_err() {
            ChainError::Altered { index, .. } => assert_eq!(index, victim),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn a_self_consistent_replacement_is_caught_by_the_next_header() {
    let s = five();
    // Same positions, different content, validly sealed: only the next header's link shows it.
    let fake = {
        let mut cfg = crate::SegmentConfig::after("tenant-a", 1, s[0].hash);
        cfg.block_target = 200;
        let mut w = crate::SegmentWriter::new(cfg).expect("cfg");
        for q in 11..=20u64 {
            w.push(super::pos(1, q), b"{\"forged\":true}")
                .expect("push");
        }
        w.seal(["event/1"]).expect("seal")
    };
    let e = verify_chain(&bytes(&[&s[0], &fake, &s[2]])).unwrap_err();
    match e {
        ChainError::Altered { index, .. } => assert_eq!(index, 1),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_generation_going_backwards_or_a_new_source_is_refused() {
    let a = build("tenant-a", 2, 1, 3, None, 200);
    let back = build("tenant-a", 1, 4, 3, Some(a.hash), 200);
    assert_eq!(
        verify_chain(&bytes(&[&a, &back])).unwrap_err(),
        ChainError::GenerationMismatch { index: 1 }
    );
    let other = build("tenant-b", 2, 4, 3, Some(a.hash), 200);
    assert_eq!(
        verify_chain(&bytes(&[&a, &other])).unwrap_err(),
        ChainError::SourceMismatch { index: 1 }
    );
}

#[test]
fn a_new_generation_may_restart_the_sequence() {
    let a = build("tenant-a", 1, 1, 3, None, 200);
    let b = build("tenant-a", 2, 1, 3, Some(a.hash), 200);
    let r = verify_chain(&bytes(&[&a, &b])).expect("chain");
    assert_eq!(r.entries, 6);
    assert_eq!(r.last.map(|p| (p.generation, p.seq)), Some((2, 3)));
}

#[test]
fn a_hole_in_positions_with_a_valid_link_is_a_position_gap() {
    let a = build("tenant-a", 1, 1, 3, None, 200);
    let b = build("tenant-a", 1, 10, 3, Some(a.hash), 200);
    assert_eq!(
        verify_chain(&bytes(&[&a, &b])).unwrap_err(),
        ChainError::PositionGap { index: 1 }
    );
}

#[test]
fn an_empty_segment_links_but_carries_no_positions() {
    let a = build("tenant-a", 1, 1, 3, None, 200);
    let e = build("tenant-a", 1, 0, 0, Some(a.hash), 200);
    let c = build("tenant-a", 1, 4, 3, Some(e.hash), 200);
    let r = verify_chain(&bytes(&[&a, &e, &c])).expect("chain");
    assert_eq!((r.segments, r.entries), (3, 6));
}

#[test]
fn a_chain_may_continue_a_known_segment() {
    let s = five();
    let tail = bytes(&[&s[3], &s[4]]);
    assert!(verify_chain_from(Some(s[2].hash), &tail).is_ok());
    assert_eq!(
        verify_chain(&tail).unwrap_err(),
        ChainError::Missing { index: 0 }
    );
    assert_eq!(
        verify_chain_from(Some(s[1].hash), &tail).unwrap_err(),
        ChainError::Missing { index: 0 }
    );
}

#[test]
fn chain_error_codes_are_catalogue_codes() {
    for (e, c) in [
        (
            ChainError::Altered {
                index: 0,
                reason: String::new(),
            },
            "journal_segment.chain_altered",
        ),
        (
            ChainError::Missing { index: 0 },
            "journal_segment.chain_missing",
        ),
        (
            ChainError::Reordered { index: 0 },
            "journal_segment.chain_reordered",
        ),
        (
            ChainError::PositionGap { index: 0 },
            "journal_segment.chain_position_gap",
        ),
        (
            ChainError::GenerationMismatch { index: 0 },
            "journal_segment.chain_generation_mismatch",
        ),
        (
            ChainError::SourceMismatch { index: 0 },
            "journal_segment.chain_source_mismatch",
        ),
    ] {
        assert_eq!(e.code(), c);
    }
}
