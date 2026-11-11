//! A clean journal and the ways a journal loses or changes data.

use spot_journal_store::JournalStore;

use super::*;
use crate::{Mode, verify_tenant};

fn run(store: &impl JournalStore, mode: Mode) -> crate::VerifyReport {
    block_on(verify_tenant(store, &keys(), &expected(None), mode)).unwrap()
}

#[test]
fn a_clean_journal_has_no_findings_in_either_mode() {
    let journal = Journal::new(4);
    let store = journal.store();
    let manifest_len = journal.signed().to_bytes().len() as u64;

    let light = run(&store, Mode::Light);
    assert!(light.is_clean(), "{:?}", light.findings);
    assert_eq!(light.generation, 1);
    assert_eq!(light.segments_listed, 4);
    assert_eq!(light.segments_examined, 4);
    assert_eq!(
        light.bytes_read, manifest_len,
        "light mode reads no segment"
    );
    assert!(!light.partial);
    assert_eq!(light.cursor.verified_segments(), 4);
    assert_eq!(light.cursor.last_seq(), 12);

    let deep = run(&store, Mode::Deep);
    assert!(deep.is_clean(), "{:?}", deep.findings);
    let segment_bytes: u64 = journal.segments.iter().map(|s| s.bytes.len() as u64).sum();
    assert_eq!(deep.bytes_read, manifest_len + segment_bytes);
}

#[test]
fn a_deleted_segment_is_reported_with_its_range_in_both_modes() {
    let journal = Journal::new(4);
    let store = journal.store_with(|index, bytes| (index != 1).then_some(bytes));
    let (first, last) = journal.range(1);
    for mode in [Mode::Light, Mode::Deep] {
        let report = run(&store, mode);
        assert_eq!(kinds(&report), vec![FindingKind::MissingSegment]);
        let finding = &report.findings[0];
        assert_eq!(
            (
                finding.tenant.as_str(),
                finding.generation,
                finding.first_seq,
                finding.last_seq
            ),
            (TENANT, 1, first, last)
        );
        assert!(finding.is_tampering());
        assert_eq!(
            report.cursor.verified_segments(),
            1,
            "the cursor stops before the hole"
        );
    }
}

#[test]
fn a_flipped_byte_in_a_block_is_found_deeply_and_by_hash_lightly() {
    let journal = Journal::new(3);
    let store = journal.store_with(|index, mut bytes| {
        if index == 1 {
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0x01;
        }
        Some(bytes)
    });
    let (first, last) = journal.range(1);

    let light = run(&store, Mode::Light);
    assert_eq!(kinds(&light), vec![FindingKind::HashMismatch]);
    assert_eq!(
        (light.findings[0].first_seq, light.findings[0].last_seq),
        (first, last)
    );

    let deep = run(&store, Mode::Deep);
    let found = kinds(&deep);
    assert!(found.contains(&FindingKind::HashMismatch), "{found:?}");
    assert!(found.contains(&FindingKind::SegmentCorrupt), "{found:?}");
    assert!(deep.has_tampering());
    assert_eq!(deep.cursor.verified_segments(), 1);
}

#[test]
fn a_segment_whose_header_disagrees_with_the_manifest_is_a_header_mismatch() {
    // A segment of the right source and chain position but fewer entries, listed under the range
    // the manifest gives it: sealed again with the same previous hash.
    let journal = Journal::new(3);
    let short = seal_short(&journal, 1);
    let store =
        journal.store_with(|index, bytes| Some(if index == 1 { short.clone() } else { bytes }));
    let report = run(&store, Mode::Deep);
    let found = kinds(&report);
    assert!(found.contains(&FindingKind::HeaderMismatch), "{found:?}");
    assert!(found.contains(&FindingKind::HashMismatch), "{found:?}");
}

/// Segment `index` of `journal` resealed with one entry fewer.
fn seal_short(journal: &Journal, index: usize) -> Vec<u8> {
    use spot_events_contract::envelope::Position;
    use spot_journal_segment::{SegmentConfig, SegmentWriter};
    let header = &journal.segments[index].header;
    let mut writer = SegmentWriter::new(SegmentConfig::after(SOURCE, 1, header.prev_hash)).unwrap();
    for seq in header.first_seq..header.last_seq {
        writer
            .push(Position { generation: 1, seq }, b"{\"short\":true}")
            .unwrap();
    }
    writer.seal(["v1"]).unwrap().bytes
}

#[test]
fn a_segment_that_chains_to_other_bytes_is_a_broken_chain() {
    // Segment 1 resealed with the same entry count and range but a different previous hash.
    use spot_events_contract::envelope::Position;
    use spot_journal_segment::{SegmentConfig, SegmentWriter};
    let journal = Journal::new(3);
    let header = &journal.segments[1].header;
    let mut writer = SegmentWriter::new(SegmentConfig::after(SOURCE, 1, [9; 32])).unwrap();
    for seq in header.first_seq..=header.last_seq {
        writer
            .push(Position { generation: 1, seq }, b"{\"chain\":true}")
            .unwrap();
    }
    let swapped = writer.seal(["v1"]).unwrap().bytes;
    let store =
        journal.store_with(|index, bytes| Some(if index == 1 { swapped.clone() } else { bytes }));
    let report = run(&store, Mode::Deep);
    assert!(
        kinds(&report).contains(&FindingKind::ChainBroken),
        "{:?}",
        kinds(&report)
    );
}

#[test]
fn an_unlisted_segment_is_an_anomaly_and_a_tail_chunk_is_never_one() {
    let journal = Journal::new(2);
    let store = journal.store();
    // The next sealed segment, written but not yet in the manifest, and a tail chunk.
    let chained = {
        use spot_events_contract::envelope::Position;
        use spot_journal_segment::{SegmentConfig, SegmentWriter};
        let mut writer =
            SegmentWriter::new(SegmentConfig::after(SOURCE, 1, journal.segments[1].hash)).unwrap();
        for seq in 7..10 {
            writer
                .push(Position { generation: 1, seq }, b"{\"n\":1}")
                .unwrap();
        }
        writer.seal(["v1"]).unwrap()
    };
    block_on(store.put_new(&segment_key(&chained), chained.bytes.clone())).unwrap();
    let tail = Layout::tail_chunk(TENANT, 1, 3).unwrap().to_key();
    block_on(store.put_new(&tail, b"entries not sealed yet".to_vec())).unwrap();

    for mode in [Mode::Light, Mode::Deep] {
        let report = run(&store, mode);
        assert_eq!(
            kinds(&report),
            vec![FindingKind::UnlistedSegment],
            "{mode:?}"
        );
        let finding = &report.findings[0];
        assert_eq!((finding.first_seq, finding.last_seq), (7, 9));
        assert!(!finding.is_tampering());
        assert!(!report.has_tampering());
        assert_eq!(
            report.cursor.verified_segments(),
            2,
            "an anomaly does not stop the cursor"
        );
    }
}

#[test]
fn a_segment_moved_to_another_position_is_reordered() {
    // Segment 2's bytes are also stored under a range the manifest does not list.
    let journal = Journal::new(2);
    let store = journal.store();
    let moved = Layout::segment(TENANT, 1, 20, 22).unwrap().to_key();
    block_on(store.put_new(&moved, journal.segments[1].bytes.clone())).unwrap();
    let report = run(&store, Mode::Light);
    assert!(
        kinds(&report).contains(&FindingKind::Reordered),
        "{:?}",
        kinds(&report)
    );
    assert!(report.has_tampering());
}
