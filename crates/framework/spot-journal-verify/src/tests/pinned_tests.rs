//! The pinned form: the generation is mandatory, a rollback is never accepted, a stale pin is
//! reported.

use spot_journal_store::JournalStore;

use super::*;
use crate::{
    Mode, PinnedExpected, VerifyError, VerifyOptions, verify_pinned, verify_pinned_with,
    verify_tenant, verify_tenant_with,
};

fn pin(generation: u64) -> PinnedExpected {
    expected(None).pin(generation)
}

fn put(store: &MemoryStore, generation: u64, bytes: Vec<u8>) {
    block_on(store.put_new(&manifest_key(generation), bytes)).unwrap();
}

#[test]
fn a_pinned_verify_of_a_clean_journal_equals_the_unpinned_one() {
    let journal = Journal::new(3);
    let store = journal.store();
    for mode in [Mode::Light, Mode::Deep] {
        let pinned = block_on(verify_pinned(&store, &keys(), &pin(1), mode)).unwrap();
        let loose = block_on(verify_tenant(&store, &keys(), &expected(None), mode)).unwrap();
        assert!(pinned.is_clean(), "{:?}", pinned.findings);
        assert!(pinned.pinned);
        assert!(!loose.pinned);
        let mut normalised = pinned.clone();
        normalised.pinned = false;
        assert_eq!(normalised, loose);
    }
}

#[test]
fn the_rollback_is_not_accepted_when_the_pinned_manifest_is_gone() {
    let journal = Journal::new(2);
    let store = MemoryStore::new();
    // Generations 1 and 2 are present, generation 3 (the pinned one) was deleted.
    put(&store, 1, journal.signed().to_bytes());
    put(&store, 2, journal.signed().to_bytes());
    for segment in &journal.segments {
        block_on(store.put_new(&segment_key(segment), segment.bytes.clone())).unwrap();
    }

    let pinned = block_on(verify_pinned(&store, &keys(), &pin(3), Mode::Deep)).unwrap();
    assert!(!pinned.is_clean());
    assert_eq!(kinds(&pinned), vec![FindingKind::MissingManifest]);
    assert_eq!(pinned.generation, 3);
    assert!(pinned.has_tampering());

    // The discovery form silently moves to the newest manifest the listing shows (generation 2).
    let loose = block_on(verify_tenant(&store, &keys(), &expected(None), Mode::Deep)).unwrap();
    assert_eq!(loose.generation, 2);
}

#[test]
fn a_real_older_manifest_passes_unpinned_but_not_pinned_past_it() {
    let journal = Journal::new(2);
    let store = journal.store(); // generation 1 only
    let loose = block_on(verify_tenant(&store, &keys(), &expected(None), Mode::Deep)).unwrap();
    assert!(loose.is_clean());
    let pinned = block_on(verify_pinned(&store, &keys(), &pin(2), Mode::Deep)).unwrap();
    assert_eq!(kinds(&pinned), vec![FindingKind::MissingManifest]);
    assert_eq!(pinned.generation, 2);
}

#[test]
fn a_stale_pin_reports_the_newer_generation() {
    let journal = Journal::new(2);
    let store = journal.store();
    put(&store, 2, b"a newer manifest".to_vec());
    let report = block_on(verify_pinned(&store, &keys(), &pin(1), Mode::Deep)).unwrap();
    assert_eq!(kinds(&report), vec![FindingKind::NewerGenerationPresent]);
    let finding = &report.findings[0];
    assert_eq!(finding.generation, 2);
    assert_eq!(finding.tenant, TENANT);
    assert_eq!(finding.code(), "journal_verify.newer_generation_present");
    assert!(!finding.is_tampering());
    assert!(!report.has_tampering());
    assert!(!report.is_clean());
    // The pinned generation itself was verified in full.
    assert_eq!(report.generation, 1);
    assert_eq!(report.segments_examined, 2);
}

#[test]
fn a_missing_pinned_manifest_and_a_newer_one_report_both() {
    let journal = Journal::new(1);
    let store = MemoryStore::new();
    put(&store, 4, b"newer".to_vec());
    let report = block_on(verify_pinned(&store, &keys(), &pin(3), Mode::Light)).unwrap();
    assert_eq!(
        kinds(&report),
        vec![
            FindingKind::MissingManifest,
            FindingKind::NewerGenerationPresent
        ]
    );
    drop(journal);
}

#[test]
fn the_unpinned_form_never_reports_a_newer_generation() {
    let journal = Journal::new(1);
    let store = journal.store();
    put(&store, 2, b"newer".to_vec());
    let report = block_on(verify_tenant(&store, &keys(), &expected(None), Mode::Light)).unwrap();
    // It verifies generation 2, which is not a manifest, and never mentions a newer one.
    assert!(
        !kinds(&report).contains(&FindingKind::NewerGenerationPresent),
        "{:?}",
        report.findings
    );
}

#[test]
fn generation_zero_and_an_empty_source_are_refused() {
    let store = MemoryStore::new();
    let zero = block_on(verify_pinned(&store, &keys(), &pin(0), Mode::Light));
    assert!(matches!(zero, Err(VerifyError::ExpectedInvalid(_))));
    let mut empty = pin(1);
    empty.source = " ".to_owned();
    let refused = block_on(verify_pinned(&store, &keys(), &empty, Mode::Light));
    assert!(matches!(refused, Err(VerifyError::ExpectedInvalid(_))));
    assert!(PinnedExpected::new(TenantId::new(TENANT).unwrap(), SOURCE, 0).is_err());
    assert!(PinnedExpected::new(TenantId::new(TENANT).unwrap(), "", 1).is_err());
    let good = PinnedExpected::new(TenantId::new(TENANT).unwrap(), SOURCE, 2).unwrap();
    assert_eq!(good.generation, 2);
}

#[test]
fn a_cursor_of_a_pinned_pass_resumes_a_pinned_pass_and_is_void_for_another_generation() {
    let journal = Journal::new(4);
    let store = CountingStore::new(journal.store());
    let first = block_on(verify_pinned(&store, &keys(), &pin(1), Mode::Deep)).unwrap();
    assert!(first.is_clean());
    assert_eq!(first.cursor.generation(), 1);
    assert_eq!(first.cursor.verified_segments(), 4);

    store.clear();
    let again = block_on(verify_pinned_with(
        &store,
        &keys(),
        &pin(1),
        Mode::Deep,
        VerifyOptions {
            cursor: Some(first.cursor.clone()),
            ..VerifyOptions::default()
        },
    ))
    .unwrap();
    assert!(again.is_clean());
    assert_eq!(again.segments_skipped, 4);
    assert_eq!(again.segments_examined, 0);
    assert_eq!(store.reads_of(&segment_key(&journal.segments[0])), 0);

    // A cursor that belongs to another generation is void: nothing is skipped.
    let other = block_on(verify_tenant_with(
        &store,
        &keys(),
        &expected(Some(1)),
        Mode::Deep,
        VerifyOptions::default(),
    ))
    .unwrap();
    let mut foreign_cursor = other.cursor.clone();
    foreign_cursor.generation = 2;
    let voided = block_on(verify_pinned_with(
        &store,
        &keys(),
        &pin(1),
        Mode::Deep,
        VerifyOptions {
            cursor: Some(foreign_cursor),
            ..VerifyOptions::default()
        },
    ))
    .unwrap();
    assert_eq!(voided.segments_skipped, 0);
    assert_eq!(voided.segments_examined, 4);
    assert!(voided.is_clean());
}
