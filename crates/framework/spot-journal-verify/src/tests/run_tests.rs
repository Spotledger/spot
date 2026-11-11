//! Incremental runs, the byte budget and the cursor.

use spot_journal_manifest::{Manifest, append_segments};
use spot_journal_store::JournalStore;

use super::*;
use crate::{Mode, VerifyError, VerifyOptions, verify_tenant_with};

fn run_with(
    store: &impl JournalStore,
    mode: Mode,
    options: VerifyOptions,
) -> Result<crate::VerifyReport, VerifyError> {
    block_on(verify_tenant_with(
        store,
        &keys(),
        &expected(None),
        mode,
        options,
    ))
}

#[test]
fn a_budget_stops_the_pass_and_the_cursor_resumes_it() {
    let journal = Journal::new(6);
    let store = journal.store();
    let manifest_len = journal.signed().to_bytes().len() as u64;
    let segment_len = journal.segments[0].bytes.len() as u64;
    // Room for the manifest and about two segments.
    let budget = manifest_len + segment_len * 2 + segment_len / 2;

    let mut cursor = None;
    let mut examined = 0;
    let mut runs = 0;
    loop {
        runs += 1;
        assert!(runs <= 10, "the passes must make progress");
        let report = run_with(
            &store,
            Mode::Deep,
            VerifyOptions {
                budget_bytes: budget,
                cursor,
            },
        )
        .unwrap();
        assert!(
            report.bytes_read <= budget,
            "{} > {budget}",
            report.bytes_read
        );
        assert!(report.is_clean(), "{:?}", report.findings);
        examined += report.segments_examined;
        let partial = report.partial;
        cursor = Some(report.cursor);
        if !partial {
            break;
        }
        assert!(report.segments_examined >= 1);
    }
    assert!(runs >= 3, "six segments do not fit one pass: {runs}");
    assert_eq!(
        examined, 6,
        "every segment is read exactly once across the passes"
    );
    assert_eq!(cursor.unwrap().verified_segments(), 6);
}

#[test]
fn a_budget_that_cannot_hold_the_next_object_is_refused() {
    let journal = Journal::new(2);
    let store = journal.store();
    let tiny = VerifyOptions {
        budget_bytes: 10,
        cursor: None,
    };
    let refused = run_with(&store, Mode::Light, tiny.clone()).unwrap_err();
    assert!(matches!(refused, VerifyError::BudgetTooSmall { .. }));
    assert_eq!(refused.code(), "journal_verify.budget_too_small");

    let manifest_len = journal.signed().to_bytes().len() as u64;
    let no_room = VerifyOptions {
        budget_bytes: manifest_len + 10,
        cursor: None,
    };
    assert!(matches!(
        run_with(&store, Mode::Deep, no_room),
        Err(VerifyError::BudgetTooSmall { .. })
    ));
    // Light mode reads only the manifest, so that budget is enough for it.
    let enough = VerifyOptions {
        budget_bytes: manifest_len,
        cursor: None,
    };
    assert!(run_with(&store, Mode::Light, enough).unwrap().is_clean());
}

#[test]
fn a_second_run_reads_nothing_it_already_verified() {
    let journal = Journal::new(4);
    let counting = CountingStore::new(journal.store());
    let first = run_with(&counting, Mode::Deep, VerifyOptions::default()).unwrap();
    assert!(first.is_clean());
    for segment in &journal.segments {
        assert_eq!(counting.reads_of(&segment_key(segment)), 1);
    }

    // The relay seals one more segment and publishes a longer manifest.
    let more = {
        use spot_events_contract::envelope::Position;
        use spot_journal_segment::{SegmentConfig, SegmentWriter};
        let last = journal.segments.last().unwrap();
        let mut writer = SegmentWriter::new(SegmentConfig::after(SOURCE, 1, last.hash)).unwrap();
        for seq in 13..16 {
            writer
                .push(Position { generation: 1, seq }, b"{\"n\":1}")
                .unwrap();
        }
        writer.seal(["v1"]).unwrap()
    };
    block_on(counting.put_new(&segment_key(&more), more.bytes.clone())).unwrap();
    let grown = append_segments(&journal.manifest, &[summary(&more)]).unwrap();
    let etag = block_on(counting.head(&manifest_key(1)))
        .unwrap()
        .unwrap()
        .etag;
    let signed = spot_journal_manifest::sign(&grown, &signing_key(1), KEY_ID);
    block_on(counting.put_replace(&manifest_key(1), signed.to_bytes(), &etag)).unwrap();

    counting.clear();
    let options = VerifyOptions {
        cursor: Some(first.cursor),
        ..VerifyOptions::default()
    };
    let second = run_with(&counting, Mode::Deep, options).unwrap();
    assert!(second.is_clean(), "{:?}", second.findings);
    assert_eq!((second.segments_skipped, second.segments_examined), (4, 1));
    for segment in &journal.segments {
        assert_eq!(
            counting.reads_of(&segment_key(segment)),
            0,
            "already verified"
        );
    }
    assert_eq!(counting.reads_of(&segment_key(&more)), 1);
    assert_eq!(second.cursor.verified_segments(), 5);
    assert_eq!(second.cursor.last_seq(), 15);
}

#[test]
fn a_third_run_with_nothing_new_reads_only_the_manifest() {
    let journal = Journal::new(3);
    let counting = CountingStore::new(journal.store());
    let first = run_with(&counting, Mode::Light, VerifyOptions::default()).unwrap();
    counting.clear();
    let options = VerifyOptions {
        cursor: Some(first.cursor),
        ..VerifyOptions::default()
    };
    let again = run_with(&counting, Mode::Light, options).unwrap();
    assert_eq!((again.segments_skipped, again.segments_examined), (3, 0));
    assert_eq!(
        counting.reads.lock().unwrap().len(),
        2,
        "head and get of the manifest only"
    );
}

#[test]
fn a_manifest_that_no_longer_lists_what_was_verified_voids_the_cursor() {
    let journal = Journal::new(3);
    let first = run_with(&journal.store(), Mode::Deep, VerifyOptions::default()).unwrap();

    // Another journal of the same shape and source: the same positions, different content.
    let rewritten = Journal::salted(3, "b");
    let report = run_with(
        &rewritten.store(),
        Mode::Deep,
        VerifyOptions {
            cursor: Some(first.cursor),
            ..VerifyOptions::default()
        },
    )
    .unwrap();
    assert_eq!(report.segments_skipped, 0, "everything is read again");
    assert_eq!(report.segments_examined, 3);
    assert_eq!(kinds(&report), vec![FindingKind::ManifestRegressed]);
    assert!(report.has_tampering());
    // The pass itself found the new journal sound, so its cursor covers it.
    assert_eq!(report.cursor.verified_segments(), 3);
}

#[test]
fn a_manifest_rolled_back_to_fewer_segments_is_a_regression() {
    let journal = Journal::new(4);
    let first = run_with(&journal.store(), Mode::Light, VerifyOptions::default()).unwrap();

    let shorter = Journal {
        segments: journal.segments[..2].to_vec(),
        manifest: append_segments(
            &Manifest::first(SOURCE, 1_000).unwrap(),
            &journal.segments[..2]
                .iter()
                .map(summary)
                .collect::<Vec<_>>(),
        )
        .unwrap(),
    };
    let report = run_with(
        &shorter.store(),
        Mode::Light,
        VerifyOptions {
            cursor: Some(first.cursor),
            ..VerifyOptions::default()
        },
    )
    .unwrap();
    assert_eq!(kinds(&report), vec![FindingKind::ManifestRegressed]);
    assert_eq!(report.segments_skipped, 0);
}

#[test]
fn a_light_cursor_does_not_excuse_a_deep_pass() {
    let journal = Journal::new(3);
    let store = journal.store();
    let light = run_with(&store, Mode::Light, VerifyOptions::default()).unwrap();
    let deep = run_with(
        &store,
        Mode::Deep,
        VerifyOptions {
            cursor: Some(light.cursor),
            ..VerifyOptions::default()
        },
    )
    .unwrap();
    assert_eq!((deep.segments_skipped, deep.segments_examined), (0, 3));
}

#[test]
fn a_cursor_about_another_tenant_or_source_is_ignored() {
    let journal = Journal::new(3);
    let store = journal.store();
    let first = run_with(&store, Mode::Light, VerifyOptions::default()).unwrap();
    let mut other = expected(None);
    other.source = "site-9".to_owned();
    let mut cursor = first.cursor;
    cursor.source = other.source;
    let report = run_with(
        &store,
        Mode::Light,
        VerifyOptions {
            cursor: Some(cursor),
            ..VerifyOptions::default()
        },
    )
    .unwrap();
    assert_eq!(report.segments_skipped, 0);
    assert!(report.is_clean());
}
