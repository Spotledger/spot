//! Verification, tenancy, cancel and sink failure.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_journal_rebuild_plan::Target;
use spot_journal_store::{
    JournalStore, Layout, MemoryStore, ObjectEntry, ObjectMeta, StoreFuture, StoredObject, TenantId,
};

use super::block_on;
use super::fixture::{Journal, SOURCE, TENANT, insert, line, options, segment_key, update};
use crate::{ApplySink, RebuildError, RebuildKeys, RecordingSink, execute};

fn run(
    journal: &Journal,
    target: Target,
    sink: &mut RecordingSink,
    opts: &crate::RebuildOptions<'_>,
) -> Result<crate::RebuildOutcome, RebuildError> {
    let plan = journal.plan(vec![], target);
    block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        sink,
        opts,
    ))
}

fn events(count: u64) -> Vec<super::fixture::Ev> {
    (1..=count)
        .map(|n| {
            if n <= 4 {
                insert(n, 1)
            } else {
                update(1 + n % 4, 1 + n / 4)
            }
        })
        .collect()
}

/// Replaces the bytes of the journal's first segment in a copy of its store.
fn tampered(journal: &Journal) -> MemoryStore {
    let copy = MemoryStore::new();
    let first = segment_key(TENANT, &journal.segments[0]);
    for entry in block_on(journal.store.list("", None, 10_000)).unwrap() {
        let mut bytes = block_on(journal.store.get(&entry.key))
            .unwrap()
            .unwrap()
            .bytes;
        if entry.key == first {
            let at = bytes.len() / 2;
            bytes[at] ^= 0xff;
        }
        block_on(copy.put_new(&entry.key, bytes)).unwrap();
    }
    copy
}

#[test]
fn a_journal_that_does_not_verify_stops_before_any_apply() {
    let journal = Journal::new(events(12), 4);
    let plan = journal.plan(vec![], Target::End);
    let store = tampered(&journal);
    let mut sink = RecordingSink::new();
    let error = block_on(execute(
        &store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &options(1),
    ))
    .unwrap_err();
    assert!(
        matches!(
            error,
            RebuildError::VerifyFailed {
                kind: "journal_verify.hash_mismatch",
                generation: 1,
                first_seq: 1,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(error.code(), "journal_rebuild.verify_failed");
    assert!(sink.calls().is_empty());
}

#[test]
fn skipping_verification_still_checks_every_segment_hash_before_applying() {
    let journal = Journal::new(events(12), 4);
    let plan = journal.plan(vec![], Target::End);
    let store = tampered(&journal);
    let mut sink = RecordingSink::new();
    let mut opts = options(1);
    opts.skip_verify = true;
    let error = block_on(execute(
        &store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &opts,
    ))
    .unwrap_err();
    assert!(
        matches!(
            error,
            RebuildError::SegmentInvalid {
                generation: 1,
                first_seq: 1,
                last_seq: 4,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(sink.calls().is_empty());
}

#[test]
fn a_manifest_signed_by_an_untrusted_key_is_refused() {
    let journal = Journal::new(events(8), 4);
    let plan = journal.plan(vec![], Target::End);
    let empty = spot_journal_manifest::VerifyingKeys::new();
    let keys = RebuildKeys {
        manifest: &empty,
        snapshot: &super::fixture::Toy,
    };
    let mut sink = RecordingSink::new();
    let mut opts = options(1);
    opts.skip_verify = true;
    let error = block_on(execute(
        &journal.store,
        &keys,
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &opts,
    ))
    .unwrap_err();
    assert!(
        matches!(error, RebuildError::ManifestInvalid { generation: 1, .. }),
        "{error:?}"
    );
    assert!(sink.calls().is_empty());
}

#[test]
fn a_journal_of_another_source_is_refused() {
    let journal = Journal::build(TENANT, "other.example", events(8), 4);
    let mut sink = RecordingSink::new();
    let mut opts = options(1);
    opts.skip_verify = true;
    let error = run(&journal, Target::End, &mut sink, &opts).unwrap_err();
    assert!(
        matches!(error, RebuildError::ManifestInvalid { generation: 1, .. }),
        "{error:?}"
    );
    assert!(sink.calls().is_empty());
}

#[test]
fn an_entry_of_another_source_inside_a_valid_segment_never_reaches_the_sink() {
    // The segment header and the manifest name the right source; one envelope does not.
    let script = events(6);
    let journal = Journal::new(script, 6);
    let position = spot_events_contract::envelope::Position {
        generation: 1,
        seq: 3,
    };
    let foreign = line(&insert(99, 1), position, "other.example");
    assert!(foreign.contains("other.example"));
    // Rebuild the one segment with the foreign line at position 3.
    let mut config = spot_journal_segment::SegmentConfig::first(SOURCE, 1);
    config.block_target = 400;
    let mut writer = spot_journal_segment::SegmentWriter::new(config).unwrap();
    for seq in 1..=6_u64 {
        let position = spot_events_contract::envelope::Position { generation: 1, seq };
        let text = if seq == 3 {
            foreign.clone()
        } else {
            line(
                &journal.events[usize::try_from(seq).unwrap() - 1],
                position,
                SOURCE,
            )
        };
        writer.push(position, text.as_bytes()).unwrap();
    }
    let sealed = writer.seal(["v1"]).unwrap();
    let manifest = spot_journal_manifest::append_segments(
        &spot_journal_manifest::Manifest::first(SOURCE, 1_000).unwrap(),
        &[super::fixture::summary(&sealed)],
    )
    .unwrap();
    let signed = spot_journal_manifest::sign(
        &manifest,
        &super::fixture::signing_key(),
        super::fixture::KEY_ID,
    );
    let store = MemoryStore::new();
    block_on(store.put_new(
        &Layout::manifest(TENANT, 1).unwrap().to_key(),
        signed.to_bytes(),
    ))
    .unwrap();
    block_on(store.put_new(&segment_key(TENANT, &sealed), sealed.bytes.clone())).unwrap();
    let plan = journal.plan(vec![], Target::End);
    let mut sink = RecordingSink::new();
    let error = block_on(execute(
        &store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &options(1),
    ))
    .unwrap_err();
    assert_eq!(
        error,
        RebuildError::ForeignEntry {
            generation: 1,
            seq: 3
        }
    );
    assert!(
        sink.calls().is_empty(),
        "the prescan stops it before the first apply"
    );
}

/// A store that remembers every key read.
struct Spy {
    inner: MemoryStore,
    reads: Mutex<Vec<String>>,
}

impl JournalStore for Spy {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        self.inner.put_new(key, bytes)
    }
    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta> {
        self.inner.put_replace(key, bytes, etag)
    }
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        self.reads.lock().unwrap().push(key.to_owned());
        self.inner.get(key)
    }
    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        self.reads.lock().unwrap().push(key.to_owned());
        self.inner.head(key)
    }
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>> {
        self.reads.lock().unwrap().push(format!("list:{prefix}"));
        self.inner.list(prefix, after, limit)
    }
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.inner.delete(key)
    }
}

#[test]
fn another_tenants_objects_are_never_read() {
    let mine = Journal::new(events(8), 4);
    let theirs = Journal::build("evil", SOURCE, events(8), 4);
    for entry in block_on(theirs.store.list("", None, 1000)).unwrap() {
        let object = block_on(theirs.store.get(&entry.key)).unwrap().unwrap();
        block_on(mine.store.put_new(&entry.key, object.bytes)).unwrap();
    }
    let spy = Spy {
        inner: mine.store,
        reads: Mutex::new(Vec::new()),
    };
    let journal = Journal {
        store: MemoryStore::new(),
        ..mine
    };
    let plan = journal.plan(vec![], Target::End);
    let mut sink = RecordingSink::new();
    block_on(execute(
        &spy,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &options(1),
    ))
    .unwrap();
    let reads = spy.reads.lock().unwrap();
    assert!(!reads.is_empty());
    assert!(
        reads
            .iter()
            .all(|k| k.starts_with("acme/") || k.starts_with("list:acme/")),
        "{reads:?}"
    );
}

#[test]
fn a_snapshot_copied_from_another_tenants_prefix_is_refused() {
    let journal = Journal::new(events(12), 4);
    let snapshot = journal.snapshot(4);
    // Move the acme snapshot's objects under another tenant's prefix and ask as that tenant.
    let moved = MemoryStore::new();
    for entry in block_on(journal.store.list("acme/snapshots/", None, 1000)).unwrap() {
        let object = block_on(journal.store.get(&entry.key)).unwrap().unwrap();
        block_on(moved.put_new(&entry.key.replacen("acme/", "evil/", 1), object.bytes)).unwrap();
    }
    for entry in block_on(journal.store.list("", None, 1000)).unwrap() {
        if !entry.key.contains("/snapshots/") {
            let object = block_on(journal.store.get(&entry.key)).unwrap().unwrap();
            block_on(moved.put_new(&entry.key.replacen("acme/", "evil/", 1), object.bytes))
                .unwrap();
        }
    }
    let plan = journal.plan(vec![snapshot], Target::End);
    let mut opts = options(1);
    opts.tenant = TenantId::new("evil").unwrap();
    opts.skip_verify = true;
    let mut sink = RecordingSink::new();
    let error = block_on(execute(
        &moved,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        &plan,
        &mut sink,
        &opts,
    ))
    .unwrap_err();
    // The manifest was signed for another source/tenant layout or the snapshot index names acme:
    // either way nothing is applied.
    assert!(
        matches!(
            error,
            RebuildError::SnapshotFailed {
                code: "journal_snapshot.index_misplaced"
            } | RebuildError::ManifestInvalid { .. }
        ),
        "{error:?}"
    );
    assert!(sink.calls().is_empty());
}

#[test]
fn a_cancel_aborts_the_sink_and_returns_cancelled() {
    let journal = Journal::new(events(20), 5);
    let cancel = AtomicBool::new(false);
    let report = |p: crate::Progress| {
        if p.processed >= 6 {
            cancel.store(true, Ordering::Release);
        }
    };
    let mut opts = options(1);
    opts.progress = Some(&report);
    opts.progress_every = 2;
    opts.cancel = Some(&cancel);
    let mut sink = RecordingSink::new();
    let error = run(&journal, Target::End, &mut sink, &opts).unwrap_err();
    assert_eq!(error, RebuildError::Cancelled { processed: 6 });
    assert_eq!(sink.aborted(), Some("journal_rebuild.cancelled"));
    assert!(sink.state().is_none());
}

#[test]
fn a_sink_that_refuses_aborts_with_its_reason_code() {
    let journal = Journal::new(events(10), 5);
    let mut sink = RecordingSink::failing_after(3);
    let error = run(&journal, Target::End, &mut sink, &options(1)).unwrap_err();
    assert!(matches!(error, RebuildError::SinkFailed { .. }));
    assert_eq!(sink.aborted(), Some("journal_rebuild.sink_failed"));
}

#[test]
fn the_sink_contract_is_object_safe() {
    fn takes(_: &mut dyn ApplySink) {}
    takes(&mut RecordingSink::new());
}

#[test]
fn unusable_options_are_refused() {
    let journal = Journal::new(events(4), 4);
    let mut sink = RecordingSink::new();
    let mut opts = options(1);
    opts.source = " ".into();
    assert!(matches!(
        run(&journal, Target::End, &mut sink, &opts),
        Err(RebuildError::OptionsInvalid(_))
    ));
    let report = |_: crate::Progress| {};
    let mut opts = options(1);
    opts.progress = Some(&report);
    opts.progress_every = 0;
    assert!(matches!(
        run(&journal, Target::End, &mut sink, &opts),
        Err(RebuildError::OptionsInvalid(_))
    ));
}
