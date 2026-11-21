//! The journal reader against a real signed journal in a memory store.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_event_stream_journal::{
    HistoryError, JournalHistory, JournalReader, Selector, check_seam,
};
use spot_journal_manifest::Manifest;
use spot_journal_store::{JournalStore, Layout, MemoryStore};

fn all() -> &'static Selector {
    &|_| true
}

fn journal(ranges: &[(u64, u64)]) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    let built = build(Manifest::first(SOURCE, 1).expect("manifest"), ranges);
    put_built(&store, &built);
    store
}

#[test]
fn an_empty_journal_has_no_history() {
    let store = Arc::new(MemoryStore::new());
    let r = reader(&store);
    let page = block(r.read_after(pos(0, 0), 10, None, all())).expect("page");
    assert!(page.envelopes.is_empty());
    assert!(page.reached_outbox);
    assert_eq!(block(r.first_available()).expect("first"), None);
}

#[test]
fn sealed_history_is_served_in_order_across_segments_and_pages() {
    let store = journal(&[(1, 4), (5, 9), (10, 12)]);
    let r = reader(&store);
    assert_eq!(block(r.first_available()).expect("first"), Some(pos(1, 1)));
    let mut after = pos(0, 0);
    let mut got = Vec::new();
    loop {
        let page = block(r.read_after(after, 5, None, all())).expect("page");
        got.extend(seqs(&page));
        after = page.next_after;
        if page.reached_outbox {
            break;
        }
    }
    let want: Vec<(u64, u64)> = (1..=12).map(|s| (1, s)).collect();
    assert_eq!(got, want);
}

#[test]
fn the_seam_bound_stops_history_and_marks_the_outbox() {
    let store = journal(&[(1, 10)]);
    let r = reader(&store);
    let page = block(r.read_after(pos(1, 2), 100, Some(pos(1, 6)), all())).expect("page");
    assert_eq!(seqs(&page), vec![(1, 3), (1, 4), (1, 5), (1, 6)]);
    assert_eq!(page.next_after, pos(1, 6));
    assert!(page.reached_outbox);
}

#[test]
fn the_selector_filters_but_the_checkpoint_still_advances() {
    let store = journal(&[(1, 6)]);
    let r = reader(&store);
    let none: &Selector = &|_| false;
    let page = block(r.read_after(pos(0, 0), 4, None, none)).expect("page");
    assert!(page.envelopes.is_empty());
    assert_eq!(page.next_after, pos(1, 4));
    assert!(!page.reached_outbox);
    let even: &Selector = &|e| e.position.seq % 2 == 0;
    let page = block(r.read_after(pos(0, 0), 100, None, even)).expect("page");
    assert_eq!(seqs(&page), vec![(1, 2), (1, 4), (1, 6)]);
}

#[test]
fn unsealed_chunks_beyond_the_manifest_follow_the_segments() {
    let store = journal(&[(1, 5)]);
    put_chunk(&store, 1, 1, 6, 7);
    put_chunk(&store, 1, 2, 8, 8);
    let r = reader(&store);
    let page = block(r.read_after(pos(1, 3), 100, None, all())).expect("page");
    let want: Vec<(u64, u64)> = (4..=8).map(|s| (1, s)).collect();
    assert_eq!(seqs(&page), want);
    assert!(page.reached_outbox);
}

#[test]
fn a_tail_gap_fails_closed() {
    let store = journal(&[(1, 5)]);
    put_chunk(&store, 1, 1, 8, 9);
    let r = reader(&store);
    let error = block(r.read_after(pos(0, 0), 100, None, all())).expect_err("gap");
    assert_eq!(error.code(), "eventstream_journal.history_unavailable");
}

#[test]
fn a_tampered_segment_is_never_served() {
    let store = Arc::new(MemoryStore::new());
    let built = build(
        Manifest::first(SOURCE, 1).expect("manifest"),
        &[(1, 4), (5, 8)],
    );
    // Store the second segment with one flipped bit.
    let mut tampered = built.clone_for_tamper();
    tampered.flip_bit_in_segment(1);
    put_built(&store, &tampered.built);
    let r = reader(&store);
    let error = block(r.read_after(pos(0, 0), 100, None, all())).expect_err("tampered");
    assert_eq!(error.code(), "eventstream_journal.history_unavailable");
    // Nothing from the bad range: a read that stays inside the good segment still works.
    let page = block(r.read_after(pos(0, 0), 4, Some(pos(1, 4)), all())).expect("good range");
    assert_eq!(seqs(&page).len(), 4);
}

#[test]
fn a_missing_segment_is_unavailable() {
    let store = Arc::new(MemoryStore::new());
    let built = build(
        Manifest::first(SOURCE, 1).expect("manifest"),
        &[(1, 4), (5, 8)],
    );
    // Only the manifest and the first segment.
    let (summary, bytes) = &built.segments[0];
    let key = Layout::segment(TENANT, 1, summary.first_seq, summary.last_seq)
        .expect("key")
        .to_key();
    block(store.put_new(&key, bytes.clone())).expect("put");
    put_manifest(&store, &built.manifest);
    let r = reader(&store);
    let error = block(r.read_after(pos(0, 0), 100, None, all())).expect_err("missing");
    assert_eq!(error.code(), "eventstream_journal.history_unavailable");
}

#[test]
fn a_manifest_signed_by_an_unknown_key_is_refused() {
    let store = journal(&[(1, 4)]);
    let r = JournalReader::new(
        Arc::clone(&store),
        TENANT,
        Arc::new(spot_journal_manifest::VerifyingKeys::new()),
        Arc::new(TextCodec),
    )
    .expect("reader");
    let error = block(r.read_after(pos(0, 0), 10, None, all())).expect_err("unsigned");
    assert_eq!(error.code(), "eventstream_journal.history_unavailable");
}

#[test]
fn another_tenants_objects_and_unsafe_ids_are_refused() {
    let store = Arc::new(MemoryStore::new());
    for tenant in ["../x", "a/b", ""] {
        let made = JournalReader::new(Arc::clone(&store), tenant, keys(), Arc::new(TextCodec));
        assert!(
            matches!(made, Err(HistoryError::ForeignKey)),
            "tenant {tenant:?}"
        );
    }
    // A journal written for "acme" is invisible to a reader of "other": its manifest is missing.
    let acme = journal(&[(1, 4)]);
    let other = JournalReader::new(Arc::clone(&acme), "other", keys(), Arc::new(TextCodec))
        .expect("reader");
    let page = block(other.read_after(pos(0, 0), 10, None, all())).expect("page");
    assert!(page.envelopes.is_empty());
    let error = block(other.read_after(pos(1, 2), 10, None, all())).expect_err("no manifest");
    assert_eq!(error.code(), "eventstream_journal.history_unavailable");
}

#[test]
fn a_segment_of_another_source_is_a_foreign_object() {
    let store = Arc::new(MemoryStore::new());
    let other = Manifest::first("other-site", 1).expect("manifest");
    // Segments are written for SOURCE but the manifest names another source.
    let built = build(Manifest::first(SOURCE, 1).expect("manifest"), &[(1, 3)]);
    let mut manifest = other;
    manifest.segments = built.manifest.segments.clone();
    for (summary, bytes) in &built.segments {
        let key = Layout::segment(TENANT, 1, summary.first_seq, summary.last_seq)
            .expect("key")
            .to_key();
        block(store.put_new(&key, bytes.clone())).expect("put");
    }
    put_manifest(&store, &manifest);
    let r = reader(&store);
    let error = block(r.read_after(pos(0, 0), 10, None, all())).expect_err("foreign");
    assert!(matches!(
        error,
        HistoryError::ForeignKey | HistoryError::HistoryUnavailable { .. }
    ));
}

#[test]
fn a_fork_serves_the_valid_part_then_the_next_generation_and_refuses_the_abandoned_part() {
    let store = Arc::new(MemoryStore::new());
    let g1 = build(
        Manifest::first(SOURCE, 1).expect("manifest"),
        &[(1, 4), (5, 8)],
    );
    put_built(&store, &g1);
    // Fork after position 6: positions 7 and 8 of generation 1 are abandoned.
    let forked = g1.manifest.fork(6, "restore", 5, 6).expect("fork");
    let g2 = build(forked, &[(1, 3)]);
    // Generation 2's segment lines are generation 2.
    put_built_without_manifest_dup(&store, &g2);
    let r = reader(&store);

    let page = block(r.read_after(pos(0, 0), 100, None, all())).expect("page");
    let mut want: Vec<(u64, u64)> = (1..=6).map(|s| (1, s)).collect();
    want.extend((1..=3).map(|s| (2, s)));
    assert_eq!(seqs(&page), want);
    assert!(page.reached_outbox);

    // Continuing from inside the valid part of generation 1 crosses the boundary.
    let page = block(r.read_after(pos(1, 5), 100, None, all())).expect("page");
    assert_eq!(seqs(&page)[0], (1, 6));
    assert_eq!(seqs(&page)[1], (2, 1));

    // A position in the abandoned part needs a resync.
    let error = block(r.read_after(pos(1, 7), 100, None, all())).expect_err("abandoned");
    assert_eq!(error.code(), "eventstream_journal.resync_required");
    // The fork point itself is still valid.
    assert!(block(r.read_after(pos(1, 6), 100, None, all())).is_ok());
}

#[test]
fn a_request_from_generation_zero_with_a_seq_is_invalid() {
    let store = journal(&[(1, 2)]);
    let r = reader(&store);
    let error = block(r.read_after(pos(0, 5), 10, None, all())).expect_err("invalid");
    assert_eq!(error.code(), "eventstream_journal.request_invalid");
}

#[test]
fn the_page_size_is_clamped_and_zero_means_the_default() {
    let store = journal(&[(1, 12)]);
    let r = reader(&store);
    let page = block(r.read_after(pos(0, 0), 0, None, all())).expect("page");
    assert_eq!(page.envelopes.len(), 12);
    let page = block(r.read_after(pos(0, 0), usize::MAX, None, all())).expect("page");
    assert_eq!(page.envelopes.len(), 12);
}

#[test]
fn the_seam_check_fails_closed_when_the_outbox_was_trimmed_past_the_seam() {
    // Outbox retains from 8 but history ended at 6: position 7 would be skipped.
    let error = check_seam(pos(1, 6), pos(1, 8)).expect_err("trimmed");
    assert_eq!(
        error,
        HistoryError::ResyncRequired {
            generation: 1,
            seq: 7
        }
    );
    assert!(check_seam(pos(1, 6), pos(1, 7)).is_ok());
    assert!(check_seam(pos(1, 6), pos(1, 3)).is_ok());
    // The end of a generation continues at position 1 of the next one.
    assert!(check_seam(pos(1, 6), pos(2, 1)).is_ok());
    assert!(check_seam(pos(1, 6), pos(2, 4)).is_err());
}

/// Puts a built generation's segments and manifest (the earlier generation's manifest is already
/// in the store).
fn put_built_without_manifest_dup(store: &MemoryStore, built: &Built) {
    put_built(store, built);
}
