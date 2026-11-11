//! The end-to-end proof: protect through a relay into a `MemoryStore`, snapshot, rebuild with
//! reveal, destroy a subject, rebuild again, and scan every stored object for the clear values.

use std::collections::BTreeMap;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use spot_events_contract::envelope::Position;
use spot_image_upcast::Registry;
use spot_journal_manifest::{Manifest, SignedManifest, VerifyingKeys, sign};
use spot_journal_pii::{MemoryKeys, SubjectKeys};
use spot_journal_rebuild::{
    ApplySink, RebuildError, RebuildKeys, RebuildOptions, RebuildOutcome, RecordingSink, execute,
};
use spot_journal_rebuild_plan::{JournalCatalog, SegmentRange, Target, plan};
use spot_journal_relay::{Appended, Batch, Durability, ManifestSigner, Relay, RelayConfig};
use spot_journal_segment::SegmentReader;
use spot_journal_snapshot::{
    ImageRecord, SnapshotInfo, SnapshotKeys, SnapshotParams, SnapshotReader, SnapshotSigner,
    SnapshotWriter,
};
use spot_journal_store::{JournalStore, MemoryStore, TenantId};

use super::block_on;
use super::support::{
    FlakyKeys, SOURCE, audit_line, clear_values, contact_body, domain_line, fields, nonces, parts,
    position, shared, state_line, tenant_keys, transform, uuid,
};
use crate::{AppendError, RevealSink};

const TENANT: &str = "acme";
const KEY_ID: &str = "journal-key-1";
/// A domain event's value: personal-looking, passed through, the declarer's responsibility.
const DOMAIN_VISIBLE: &str = "domain-visible@mail.example";
/// The tags of every contact body the script ever writes. Their values are what the scan hunts.
const TAGS: [&str; 8] = [
    "alice", "bruno", "carol", "dmitri", "hiro", "esther", "farid", "gwen",
];

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

struct FixedSigner;

impl ManifestSigner for FixedSigner {
    fn sign(&self, manifest: &Manifest) -> SignedManifest {
        sign(manifest, &signing_key(), KEY_ID)
    }
    fn key_id(&self) -> &str {
        KEY_ID
    }
}

fn manifest_keys() -> VerifyingKeys {
    let mut keys = VerifyingKeys::new();
    keys.add_key(KEY_ID, signing_key().verifying_key(), 1, u64::MAX)
        .unwrap();
    keys
}

/// A toy snapshot signer: the "signature" is the message reversed.
struct Toy;

impl SnapshotSigner for Toy {
    fn key_id(&self) -> &str {
        "snap-1"
    }
    fn sign(&self, message: &[u8]) -> Vec<u8> {
        message.iter().rev().copied().collect()
    }
}

impl SnapshotKeys for Toy {
    fn verify(&self, key_id: &str, _generation: u64, message: &[u8], signature: &[u8]) -> bool {
        key_id == "snap-1" && self.sign(message) == signature
    }
}

/// The twelve clear lines of the script, in position order.
fn script() -> Vec<String> {
    let body = |person: &str, tag: &str| contact_body(person, tag);
    vec![
        state_line("contact", 1, "insert", 1, &body("p1", "alice"), 1),
        state_line("contact", 2, "insert", 1, &body("p2", "bruno"), 2),
        state_line("contact", 3, "insert", 1, &body("p1", "carol"), 3),
        domain_line(1, &json!({"note": DOMAIN_VISIBLE}), 4),
        state_line("contact", 1, "update", 2, &body("p1", "dmitri"), 5),
        audit_line(6),
        state_line("contact", 4, "insert", 1, &body("p2", "hiro"), 7),
        state_line("contact", 2, "update", 2, &body("p2", "esther"), 8),
        state_line("contact", 3, "update", 2, &body("p1", "farid"), 9),
        state_line("contact", 1, "update", 3, &body("p1", "gwen"), 10),
        state_line("contact", 2, "delete", 3, &json!({"gone": true}), 11),
        state_line("ledger", 9, "insert", 1, &json!({"amount": "10.00"}), 12),
    ]
}

/// The database as of position 6: what the snapshot captures, in the clear.
fn clear_records_at_six() -> Vec<ImageRecord> {
    let record = |n: u64, version: u64, person: &str, tag: &str| ImageRecord {
        entity: "contact".to_owned(),
        id: uuid(n),
        version,
        entity_schema: 1,
        image: contact_body(person, tag),
    };
    vec![
        record(1, 2, "p1", "dmitri"),
        record(2, 1, "p2", "bruno"),
        record(3, 1, "p1", "carol"),
    ]
}

struct Journal {
    relay: Relay<MemoryStore>,
    snapshot: SnapshotInfo,
}

impl Journal {
    fn store(&self) -> &MemoryStore {
        self.relay.store()
    }
}

/// Writes the script through a relay into a `MemoryStore`, sealing after each half, and a snapshot
/// at position 6. With `protect` the lines and snapshot records go through the transforms; without
/// it they are written as they are (the control that makes the scan meaningful).
fn build(protect: bool, keys: &Arc<MemoryKeys>) -> Journal {
    block_on(async {
        let config = RelayConfig::new(TENANT, SOURCE, Durability::Local);
        let mut relay = Relay::open(
            MemoryStore::new(),
            Box::new(FixedSigner),
            manifest_keys(),
            config,
            1_000,
        )
        .await
        .unwrap();
        let nonces = nonces();
        let transform = transform(keys, &nonces);
        let parts = parts(keys, &nonces);
        let lines = script();
        for (batch_id, range) in [(1_u64, 1..=6_u64), (2, 7..=12)] {
            let entries = range
                .map(|seq| {
                    let line = lines[usize::try_from(seq).unwrap() - 1].clone();
                    (position(seq), line.into_bytes())
                })
                .collect();
            let batch = Batch { batch_id, entries };
            if protect {
                transform
                    .append_batch(&mut relay, batch, 2_000)
                    .await
                    .unwrap();
            } else {
                relay.append_batch(batch, 2_000).await.unwrap();
            }
            assert!(relay.seal_now(2_000).await.unwrap());
        }
        let params = SnapshotParams {
            tenant: TENANT.to_owned(),
            generation: 1,
            position: 6,
            manifest_hash: "ab".repeat(32),
            taken_at_millis: 1_700_000_000_000,
        };
        let records = if protect {
            parts.protect_records(clear_records_at_six()).unwrap()
        } else {
            clear_records_at_six()
        };
        let mut writer = SnapshotWriter::new(relay.store(), params, &Toy)
            .and_then(|w| w.with_part_bytes(400))
            .unwrap();
        writer.write_part(records).await.unwrap();
        let mut snapshot = writer.finish().await.unwrap();
        snapshot.verified = true;
        Journal { relay, snapshot }
    })
}

type Model = BTreeMap<(String, String), (u64, Position, Value)>;

fn model_of(sink: &RecordingSink) -> Model {
    sink.state()
        .expect("the rebuild committed")
        .iter()
        .map(|(key, recorded)| {
            let body = Value::Object(recorded.image.body().clone());
            (key.clone(), (recorded.version, recorded.position, body))
        })
        .collect()
}

fn run(
    journal: &Journal,
    with_snapshot: bool,
    sink: &mut dyn ApplySink,
) -> Result<RebuildOutcome, RebuildError> {
    let catalog = JournalCatalog {
        snapshots: if with_snapshot {
            vec![journal.snapshot]
        } else {
            Vec::new()
        },
        segments: vec![
            SegmentRange {
                generation: 1,
                first_seq: 1,
                last_seq: 6,
            },
            SegmentRange {
                generation: 1,
                first_seq: 7,
                last_seq: 12,
            },
        ],
        forks: Vec::new(),
    };
    let rebuild_plan = plan(&catalog, None, Target::End).unwrap();
    let keys = manifest_keys();
    block_on(execute(
        journal.store(),
        &RebuildKeys {
            manifest: &keys,
            snapshot: &Toy,
        },
        &Registry::new(),
        &rebuild_plan,
        sink,
        &RebuildOptions::new(TenantId::new(TENANT).unwrap(), SOURCE, 1),
    ))
}

/// A rebuild through the reveal wrapper over `keys`.
fn reveal_rebuild(
    journal: &Journal,
    with_snapshot: bool,
    keys: crate::SharedKeys,
) -> (RecordingSink, Result<RebuildOutcome, RebuildError>) {
    let mut sink = RevealSink::new(RecordingSink::new(), fields(), keys);
    let outcome = run(journal, with_snapshot, &mut sink);
    (sink.into_inner(), outcome)
}

/// Every view of every stored object: its raw bytes, every entry line of every segment decoded
/// from its zstd blocks, and every image of the snapshot as the reader returns it.
fn views(journal: &Journal) -> Vec<(String, Vec<u8>)> {
    let store = journal.store();
    let mut out = Vec::new();
    for entry in block_on(store.list("", None, 10_000)).unwrap() {
        let object = block_on(store.get(&entry.key)).unwrap().unwrap();
        out.push((format!("raw {}", entry.key), object.bytes.clone()));
        if let Ok(reader) = SegmentReader::open(object.bytes) {
            for decoded in reader.entries() {
                out.push((
                    format!("decoded entry of {}", entry.key),
                    decoded.unwrap().line.into_bytes(),
                ));
            }
        }
    }
    let reader = block_on(SnapshotReader::open(store, TENANT, 1, 6, &Toy)).unwrap();
    let mut images = reader.images();
    while let Some(record) = block_on(images.next()).unwrap() {
        out.push((
            "decoded snapshot image".to_owned(),
            serde_json::to_vec(&record).unwrap(),
        ));
    }
    out
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn every_clear_value() -> Vec<String> {
    TAGS.iter().flat_map(|tag| clear_values(tag)).collect()
}

/// Asserts that no clear value is in any view, and returns how many views it scanned.
fn assert_no_clear_value(journal: &Journal, when: &str) -> usize {
    let views = views(journal);
    let needles = every_clear_value();
    for (what, bytes) in &views {
        for needle in &needles {
            assert!(
                !contains(bytes, needle),
                "{when}: a clear value is in {what}"
            );
        }
    }
    views.len()
}

#[test]
fn the_journal_and_its_snapshot_never_hold_a_personal_value_in_clear() {
    let keys = tenant_keys(10, &["p1", "p2"]);
    let journal = build(true, &keys);
    let scanned = assert_no_clear_value(&journal, "before erasure");
    // 3 stored objects of the journal (manifest and two segments) plus 2 decoded-entry sets of 6
    // lines, the snapshot index and part, and the 3 decoded snapshot images.
    assert!(scanned >= 20, "scanned only {scanned} views");
    let decoded_entries = views(&journal)
        .into_iter()
        .filter(|(what, _)| what.starts_with("decoded entry"))
        .count();
    assert_eq!(
        decoded_entries, 12,
        "every one of the 12 entries was decoded"
    );
    // The domain event passes through as written: the declarer's responsibility.
    assert!(
        views(&journal)
            .iter()
            .any(|(what, bytes)| what.starts_with("decoded entry")
                && contains(bytes, DOMAIN_VISIBLE))
    );
}

#[test]
fn the_scan_finds_the_values_in_a_journal_written_without_the_transforms() {
    let keys = tenant_keys(10, &["p1", "p2"]);
    let control = build(false, &keys);
    let views = views(&control);
    for needle in every_clear_value() {
        assert!(
            views
                .iter()
                .any(|(what, bytes)| what.starts_with("decoded") && contains(bytes, &needle)),
            "the scan cannot see a value it should: it proves nothing"
        );
    }
}

#[test]
fn a_rebuild_with_reveal_equals_a_rebuild_of_the_clear_journal() {
    let keys = tenant_keys(10, &["p1", "p2"]);
    let protected = build(true, &keys);
    let control = build(false, &keys);
    for with_snapshot in [true, false] {
        let (sink, outcome) = reveal_rebuild(&protected, with_snapshot, shared(&keys));
        let outcome = outcome.unwrap();
        let mut clear_sink = RecordingSink::new();
        let clear_outcome = run(&control, with_snapshot, &mut clear_sink).unwrap();
        assert_eq!(
            model_of(&sink),
            model_of(&clear_sink),
            "snapshot: {with_snapshot}"
        );
        assert_eq!(outcome, clear_outcome);
        let state = model_of(&sink);
        let (version, at, body) = &state[&("contact".to_owned(), uuid(1))];
        assert_eq!((*version, *at), (3, position(10)));
        assert_eq!(*body, contact_body("p1", "gwen"));
        assert_eq!(state.len(), 4, "contact 1, 3 and 4 and the ledger entry");
    }
}

#[test]
fn erasing_a_subject_changes_nothing_but_its_erasable_values() {
    let keys = tenant_keys(10, &["p1", "p2"]);
    let journal = build(true, &keys);
    assert_no_clear_value(&journal, "before erasure");
    let (before_sink, before) = reveal_rebuild(&journal, true, shared(&keys));
    let before = before.unwrap();
    let before_model = model_of(&before_sink);

    keys.destroy("p1").unwrap();

    assert_no_clear_value(&journal, "after erasure");
    let (after_sink, after) = reveal_rebuild(&journal, true, shared(&keys));
    let after = after.unwrap();
    let after_model = model_of(&after_sink);

    // Record counts, versions, positions and links are identical, and so is every counter.
    assert_eq!(before, after);
    assert_eq!(before_model.len(), after_model.len());
    let placeholder = json!({"$erased": 1, "purpose": "notifications", "erased_at": null});
    for (key, (version, at, body)) in &before_model {
        let (after_version, after_at, after_body) = &after_model[key];
        assert_eq!((version, at), (after_version, after_at));
        let mut expected = body.clone();
        if body["person"] == "p1" {
            expected["email"] = placeholder.clone();
            expected["phones"][0]["number"] = placeholder.clone();
            expected["phones"][1]["number"] = placeholder.clone();
        }
        assert_eq!(*after_body, expected, "{key:?}");
    }
    // The legal-retention tax id and the other subject's values survive in full.
    let one = &after_model[&("contact".to_owned(), uuid(1))].2;
    assert_eq!(one["tax_id"], "TAX-gwen");
    assert_eq!(one["company"], "co-1");
    assert_eq!(
        after_model[&("contact".to_owned(), uuid(4))].2,
        contact_body("p2", "hiro")
    );
    // The erasure holds for a rebuild from the stream alone as well.
    let (stream_sink, stream) = reveal_rebuild(&journal, false, shared(&keys));
    stream.unwrap();
    assert_eq!(model_of(&stream_sink), after_model);
}

#[test]
fn a_key_store_outage_aborts_the_rebuild_and_it_succeeds_once_the_store_is_back() {
    let keys = tenant_keys(10, &["p1", "p2"]);
    let journal = build(true, &keys);
    let flaky = FlakyKeys::new(Arc::clone(&keys));
    flaky.set_down(true);
    let (sink, outcome) = reveal_rebuild(&journal, true, flaky.clone());
    match outcome.unwrap_err() {
        RebuildError::SinkFailed { reason } => {
            assert!(reason.contains("journal_protect.reveal_failed"), "{reason}");
            assert!(reason.contains("journal_pii.key_store"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert!(sink.aborted().is_some(), "the sink was told to abort");
    assert!(sink.state().is_none(), "nothing was committed");
    flaky.set_down(false);
    let (sink, outcome) = reveal_rebuild(&journal, true, flaky);
    outcome.unwrap();
    assert_eq!(model_of(&sink).len(), 4);
}

#[test]
fn another_tenants_keys_cannot_rebuild_the_journal() {
    let keys = tenant_keys(10, &["p1", "p2"]);
    let journal = build(true, &keys);
    let rival = tenant_keys(50, &["p1", "p2"]);
    let (sink, outcome) = reveal_rebuild(&journal, true, shared(&rival));
    assert!(matches!(outcome, Err(RebuildError::SinkFailed { .. })));
    assert!(sink.state().is_none());
}

#[test]
fn a_failed_protect_writes_nothing_and_the_tail_chunk_holds_no_clear_value() {
    block_on(async {
        // p9 has no key, so the second line of the batch cannot be protected.
        let keys = tenant_keys(10, &["p1"]);
        let nonces = nonces();
        let transform = transform(&keys, &nonces);
        let config = RelayConfig::new(TENANT, SOURCE, Durability::RemoteSync);
        let mut relay = Relay::open(
            MemoryStore::new(),
            Box::new(FixedSigner),
            manifest_keys(),
            config,
            1_000,
        )
        .await
        .unwrap();
        let before = relay.store().list("", None, 100).await.unwrap().len();

        let good = state_line("contact", 1, "insert", 1, &contact_body("p1", "alice"), 1);
        let bad = state_line("contact", 2, "insert", 1, &contact_body("p9", "bruno"), 2);
        let batch = Batch {
            batch_id: 1,
            entries: vec![
                (position(1), good.clone().into_bytes()),
                (position(2), bad.into_bytes()),
            ],
        };
        let error = transform
            .append_batch(&mut relay, batch, 2_000)
            .await
            .unwrap_err();
        assert!(matches!(error, AppendError::Protect(_)));
        assert_eq!(error.code(), "journal_protect.protect_failed");
        let listed = relay.store().list("", None, 100).await.unwrap();
        assert_eq!(
            listed.len(),
            before,
            "no tail chunk, no segment: nothing was written"
        );
        assert_eq!(relay.next_position(), position(1));

        // The relay never saw the failed batch, so the same batch id and positions are still free.
        let batch = Batch {
            batch_id: 1,
            entries: vec![(position(1), good.into_bytes())],
        };
        let appended = transform
            .append_batch(&mut relay, batch, 2_000)
            .await
            .unwrap();
        assert!(matches!(
            appended,
            Appended::ChunkDurable { up_to_seq: 1, .. }
        ));
        let mut chunk_bytes = 0;
        for entry in relay.store().list("", None, 100).await.unwrap() {
            let object = relay.store().get(&entry.key).await.unwrap().unwrap();
            chunk_bytes += object.bytes.len();
            for value in clear_values("alice")
                .into_iter()
                .chain(clear_values("bruno"))
            {
                assert!(
                    !contains(&object.bytes, &value),
                    "a clear value is in {}",
                    entry.key
                );
            }
        }
        assert!(chunk_bytes > 0);
        // The control: written without the transform, the same chunk holds the value in clear.
        let mut control = Relay::open(
            MemoryStore::new(),
            Box::new(FixedSigner),
            manifest_keys(),
            RelayConfig::new(TENANT, SOURCE, Durability::RemoteSync),
            1_000,
        )
        .await
        .unwrap();
        let line = state_line("contact", 1, "insert", 1, &contact_body("p1", "alice"), 1);
        control
            .append_batch(
                Batch {
                    batch_id: 1,
                    entries: vec![(position(1), line.into_bytes())],
                },
                2_000,
            )
            .await
            .unwrap();
        let mut found = false;
        for entry in control.store().list("", None, 100).await.unwrap() {
            let object = control.store().get(&entry.key).await.unwrap().unwrap();
            found |= contains(&object.bytes, "alice@mail.example");
        }
        assert!(
            found,
            "the raw scan sees a clear value in an unprotected tail chunk"
        );
    });
}
