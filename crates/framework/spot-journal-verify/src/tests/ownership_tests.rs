//! The ownership checks: the expected tenant, source and generation come from the caller.

use spot_events_contract::envelope::Position;
use spot_journal_manifest::{Manifest, append_segments, sign};
use spot_journal_segment::{SegmentConfig, SegmentWriter};
use spot_journal_store::{JournalStore, MemoryStore};

use super::*;
use crate::{Mode, VerifyError, verify_tenant};

fn run(store: &impl JournalStore, generation: Option<u64>, mode: Mode) -> crate::VerifyReport {
    block_on(verify_tenant(store, &keys(), &expected(generation), mode)).unwrap()
}

/// A store holding `journal`'s segments and `manifest_bytes` under generation `at`.
fn store_with_manifest(journal: &Journal, at: u64, manifest_bytes: Vec<u8>) -> MemoryStore {
    let store = journal.store_with(|_, bytes| Some(bytes));
    if at != 1 {
        block_on(store.put_new(&manifest_key(at), manifest_bytes)).unwrap();
    }
    store
}

#[test]
fn a_manifest_with_its_segments_out_of_order_is_rejected() {
    let journal = Journal::new(3);
    let mut reordered = journal.manifest.clone();
    reordered.segments.swap(0, 1);
    let signed = sign(&reordered, &signing_key(1), KEY_ID);
    let store = journal.store_with(|_, bytes| Some(bytes));
    // Replace the honest manifest by the reordered one.
    let honest = block_on(store.head(&manifest_key(1))).unwrap().unwrap();
    block_on(store.put_replace(&manifest_key(1), signed.to_bytes(), &honest.etag)).unwrap();

    let report = run(&store, None, Mode::Deep);
    assert_eq!(report.findings.len(), 1);
    assert!(matches!(
        report.findings[0].kind,
        FindingKind::ManifestRejected { reason } if reason.starts_with("journal_manifest.")
    ));
    assert!(report.has_tampering());
    assert_eq!(
        report.segments_examined, 0,
        "no segment is read behind a rejected manifest"
    );
}

#[test]
fn a_manifest_signed_by_an_untrusted_key_is_rejected() {
    let journal = Journal::new(2);
    let forged = sign(&journal.manifest, &signing_key(7), KEY_ID);
    let store = MemoryStore::new();
    block_on(store.put_new(&manifest_key(1), forged.to_bytes())).unwrap();
    let report = run(&store, None, Mode::Light);
    assert!(matches!(
        report.findings[0].kind,
        FindingKind::ManifestRejected { .. }
    ));
}

#[test]
fn a_manifest_copied_from_another_source_fails_though_its_signature_holds() {
    // The other source's manifest is signed with the very key this tenant trusts, and sits at
    // this tenant's manifest key.
    let other = seal("site-2", 1, 1, 2, 3, "a");
    let summaries: Vec<_> = other.iter().map(summary).collect();
    let manifest = append_segments(&Manifest::first("site-2", 1_000).unwrap(), &summaries).unwrap();
    let signed = sign(&manifest, &signing_key(1), KEY_ID);
    let store = MemoryStore::new();
    block_on(store.put_new(&manifest_key(1), signed.to_bytes())).unwrap();
    for segment in &other {
        block_on(store.put_new(&segment_key(segment), segment.bytes.clone())).unwrap();
    }

    for mode in [Mode::Light, Mode::Deep] {
        let report = run(&store, None, mode);
        assert_eq!(
            kinds(&report),
            vec![FindingKind::ForeignManifest],
            "{mode:?}"
        );
        assert!(report.findings[0].is_tampering());
        assert_eq!(report.findings[0].tenant, TENANT);
        assert_eq!(report.findings[0].generation, 1);
        assert_eq!(
            report.segments_examined, 0,
            "the foreign segments are not read"
        );
        assert_eq!(report.bytes_read, signed.to_bytes().len() as u64);
    }
}

#[test]
fn an_older_generation_manifest_under_a_newer_key_is_foreign() {
    // Generation 2 exists; its file has been replaced by the generation-1 manifest.
    let journal = Journal::new(2);
    let stale = journal.signed().to_bytes();
    let store = store_with_manifest(&journal, 2, stale);
    for generation in [None, Some(2)] {
        let report = run(&store, generation, Mode::Light);
        assert_eq!(
            kinds(&report),
            vec![FindingKind::ForeignManifest],
            "{generation:?}"
        );
        assert_eq!(report.generation, 2);
    }
}

#[test]
fn a_segment_of_another_source_in_a_listed_slot_is_foreign() {
    let journal = Journal::new(3);
    // A real segment of site-2 with the same range, written where the listed segment belongs.
    let other = seal("site-2", 1, 1, 3, 3, "a");
    let store = journal.store_with(|index, bytes| {
        Some(if index == 1 {
            other[1].bytes.clone()
        } else {
            bytes
        })
    });
    let report = run(&store, None, Mode::Deep);
    let found = kinds(&report);
    assert!(found.contains(&FindingKind::ForeignSegment), "{found:?}");
    assert!(report.has_tampering());
    // Lightly it is only visible as a different hash.
    assert!(kinds(&run(&store, None, Mode::Light)).contains(&FindingKind::HashMismatch));
}

#[test]
fn a_segment_of_another_source_placed_unlisted_in_the_prefix_is_foreign() {
    let journal = Journal::new(2);
    let store = journal.store();
    let mut writer = SegmentWriter::new(SegmentConfig::first("site-2", 1)).unwrap();
    for seq in 7..10 {
        writer
            .push(Position { generation: 1, seq }, b"{\"n\":1}")
            .unwrap();
    }
    let alien = writer.seal(["v1"]).unwrap();
    block_on(store.put_new(&segment_key(&alien), alien.bytes.clone())).unwrap();

    let report = run(&store, None, Mode::Deep);
    let found = kinds(&report);
    assert!(found.contains(&FindingKind::ForeignSegment), "{found:?}");
    assert!(found.contains(&FindingKind::UnlistedSegment), "{found:?}");
}

#[test]
fn a_tenant_with_no_manifest_is_a_missing_manifest() {
    let store = MemoryStore::new();
    let report = run(&store, None, Mode::Light);
    assert_eq!(kinds(&report), vec![FindingKind::MissingManifest]);
    assert_eq!(report.generation, 0);
    let named = run(&store, Some(3), Mode::Light);
    assert_eq!(
        (named.generation, kinds(&named)),
        (3, vec![FindingKind::MissingManifest])
    );
}

#[test]
fn the_expectation_must_be_usable() {
    let store = MemoryStore::new();
    let mut empty = expected(None);
    empty.source = "  ".to_owned();
    let refused = block_on(verify_tenant(&store, &keys(), &empty, Mode::Light));
    assert!(matches!(refused, Err(VerifyError::ExpectedInvalid(_))));
    let zero = block_on(verify_tenant(
        &store,
        &keys(),
        &expected(Some(0)),
        Mode::Light,
    ));
    assert_eq!(zero.unwrap_err().code(), "journal_verify.expected_invalid");
}

#[test]
fn a_finding_carries_no_object_bytes() {
    // Findings hold ids and numbers only, so their debug text cannot contain segment content.
    let journal = Journal::new(2);
    let store = journal.store_with(|index, mut bytes| {
        if index == 0 {
            bytes[40] ^= 0xff;
        }
        Some(bytes)
    });
    let report = run(&store, None, Mode::Deep);
    let text = format!("{:?}", report.findings);
    assert!(!text.contains("xxxx"), "{text}");
}
