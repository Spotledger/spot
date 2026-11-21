//! A fork across generations, end to end (WP-JRN-03.T10): the relay writes, the manifest records
//! the fork, the verifier checks, the planner plans and the executor rolls forward, all on the
//! numbering ADR-061 and the feed use: **positions start again at 1 in every generation**.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use ed25519_dalek::SigningKey;
use serde_json::json;
use spot_events_contract::envelope::Position;
use spot_journal_manifest::{
    Fork, Manifest, SignedManifest, Verified, VerifyingKeys, sign, verify,
};
use spot_journal_rebuild::{Call, RebuildKeys, RebuildOptions, RecordingSink, execute};
use spot_journal_rebuild_plan::{ForkInfo, JournalCatalog, PlanError, SegmentRange, Target, plan};
use spot_journal_relay::{Batch, Durability, ManifestSigner, Relay, RelayConfig};
use spot_journal_segment::SealLimits;
use spot_journal_snapshot::SnapshotKeys;
use spot_journal_store::{
    JournalStore, Layout, MemoryStore, ObjectEntry, ObjectMeta, StoreFuture, StoredObject, TenantId,
};
use spot_journal_verify::{FindingKind, Mode, PinnedExpected, verify_pinned};

const TENANT: &str = "acme";
const SOURCE: &str = "acme-site";
const KEY_ID: &str = "journal-key-1";

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

struct Signer;

impl ManifestSigner for Signer {
    fn sign(&self, manifest: &Manifest) -> SignedManifest {
        sign(manifest, &signing_key(), KEY_ID)
    }
    fn key_id(&self) -> &str {
        KEY_ID
    }
}

fn keys() -> VerifyingKeys {
    let mut keys = VerifyingKeys::new();
    keys.add_key(KEY_ID, signing_key().verifying_key(), 1, u64::MAX)
        .unwrap();
    keys
}

/// No snapshot exists in this journal: any snapshot signature is refused.
struct NoSnapshots;

impl SnapshotKeys for NoSnapshots {
    fn verify(&self, _key_id: &str, _generation: u64, _message: &[u8], _signature: &[u8]) -> bool {
        false
    }
}

/// One store shared by the relays of both generations.
#[derive(Clone, Default)]
struct SharedStore(Arc<MemoryStore>);

impl JournalStore for SharedStore {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        self.0.put_new(key, bytes)
    }
    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        expected_etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta> {
        self.0.put_replace(key, bytes, expected_etag)
    }
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        self.0.get(key)
    }
    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        self.0.head(key)
    }
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>> {
        self.0.list(prefix, after, limit)
    }
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.0.delete(key)
    }
}

fn uuid(n: u64) -> String {
    format!("01900000-0000-7000-8000-{n:012x}")
}

/// The envelope line of a `doc_state` insert at `(generation, seq)`: a document of its own.
fn line(generation: u64, seq: u64) -> Vec<u8> {
    let doc = generation * 1_000 + seq;
    json!({
        "event_id": uuid(10_000_000 * generation + seq),
        "topic": "doc_state",
        "schema_version": 1,
        "source_id": SOURCE,
        "position": { "generation": generation, "seq": seq },
        "transaction": uuid(20_000_000 * generation + seq),
        "ordinal": 0,
        "subject": { "entity": "account", "id": uuid(doc) },
        "subject_version": 1,
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": { "kind": "system", "component": "scheduler" },
            "correlation_id": uuid(30_000_000 * generation + seq)
        },
        "data": {
            "op": "insert",
            "entity_schema": 1,
            "image": { "name": format!("g{generation}-{seq}") }
        }
    })
    .to_string()
    .into_bytes()
}

fn batch(id: u64, generation: u64, first: u64, last: u64) -> Batch {
    Batch {
        batch_id: id,
        entries: (first..=last)
            .map(|seq| (Position { generation, seq }, line(generation, seq)))
            .collect(),
    }
}

fn config() -> RelayConfig {
    let mut config = RelayConfig::new(TENANT, SOURCE, Durability::Local);
    config.limits = SealLimits {
        max_bytes: u64::MAX,
        max_age_millis: u64::MAX,
    };
    config
}

/// Writes 1..=300 (sealed at 200 and at 300), forks at 200 and writes 10 more in generation 2.
async fn journal() -> SharedStore {
    let store = SharedStore::default();
    let mut first = Relay::open(store.clone(), Box::new(Signer), keys(), config(), 1_000)
        .await
        .unwrap();
    first
        .append_batch(batch(1, 1, 1, 200), 2_000)
        .await
        .unwrap();
    assert!(first.seal_now(3_000).await.unwrap());
    first
        .append_batch(batch(2, 1, 201, 300), 4_000)
        .await
        .unwrap();
    assert!(first.seal_now(5_000).await.unwrap());

    let fork = Fork::new(
        2,
        200,
        "point-in-time rebuild to before a bad import",
        6_000,
    );
    let mut second = Relay::open_fork(
        store.clone(),
        Box::new(Signer),
        keys(),
        config(),
        fork,
        7_000,
    )
    .await
    .unwrap();
    // Generation 2 starts again at position 1.
    assert_eq!(
        second.next_position(),
        Position {
            generation: 2,
            seq: 1
        }
    );
    second
        .append_batch(batch(1, 2, 1, 10), 8_000)
        .await
        .unwrap();
    assert!(second.seal_now(9_000).await.unwrap());
    store
}

async fn manifest_of(store: &SharedStore, generation: u64) -> Verified {
    let key = Layout::manifest(TENANT, generation).unwrap().to_key();
    let object = store.get(&key).await.unwrap().unwrap();
    verify(&SignedManifest::from_bytes(&object.bytes).unwrap(), &keys()).unwrap()
}

/// The catalogue a planner reads: every manifest's segments and the newest manifest's forks.
fn catalog(manifests: &[&Verified]) -> JournalCatalog {
    let mut segments = Vec::new();
    for manifest in manifests {
        segments.extend(manifest.segments().iter().map(|s| SegmentRange {
            generation: s.generation,
            first_seq: s.first_seq,
            last_seq: s.last_seq,
        }));
    }
    let forks = manifests
        .iter()
        .max_by_key(|m| m.generation())
        .map(|m| {
            m.forks()
                .iter()
                .map(|f| ForkInfo {
                    generation: f.generation,
                    from_position: f.forked_from_position,
                })
                .collect()
        })
        .unwrap_or_default();
    JournalCatalog {
        snapshots: Vec::new(),
        segments,
        forks,
    }
}

fn pin(generation: u64) -> PinnedExpected {
    PinnedExpected::new(TenantId::new(TENANT).unwrap(), SOURCE, generation).unwrap()
}

fn replay(store: &SharedStore, plan: &spot_journal_rebuild_plan::RebuildPlan) -> RecordingSink {
    let verifying = keys();
    let rebuild_keys = RebuildKeys {
        manifest: &verifying,
        snapshot: &NoSnapshots,
    };
    let options = RebuildOptions::new(TenantId::new(TENANT).unwrap(), SOURCE, 1);
    let mut sink = RecordingSink::new();
    block_on(execute(
        store,
        &rebuild_keys,
        &spot_image_upcast::Registry::new(),
        plan,
        &mut sink,
        &options,
    ))
    .unwrap();
    sink
}

fn applied(sink: &RecordingSink) -> Vec<(u64, u64)> {
    sink.calls()
        .iter()
        .filter_map(|call| match call {
            Call::Image(image) => Some((image.position.generation, image.position.seq)),
            _ => None,
        })
        .collect()
}

#[test]
fn the_manifest_the_relay_wrote_after_a_fork_starts_at_one_and_links_to_the_old_one() {
    let store = block_on(journal());
    let first = block_on(manifest_of(&store, 1));
    let second = block_on(manifest_of(&store, 2));
    assert_eq!(first.last_seq(), 300);
    assert_eq!(second.segments().len(), 1);
    assert_eq!(
        (
            second.segments()[0].first_seq,
            second.segments()[0].last_seq
        ),
        (1, 10)
    );
    assert_eq!(second.forks().len(), 1);
    assert_eq!(second.forks()[0].forked_from_position, 200);
    assert_eq!(
        second.manifest().previous_manifest_hash,
        Some(first.manifest().manifest_hash())
    );
    second.manifest().validate().unwrap();
}

#[test]
fn the_verifier_accepts_the_newest_generation_and_reports_a_stale_pin_on_the_old_one() {
    let store = block_on(journal());
    for mode in [Mode::Light, Mode::Deep] {
        let newest = block_on(verify_pinned(&store, &keys(), &pin(2), mode)).unwrap();
        assert!(newest.is_clean(), "{mode:?}: {:?}", newest.findings);

        // Pinned to the abandoned generation: its own history is intact; the only finding is that
        // a newer generation exists (a stale pin, not tampering).
        let old = block_on(verify_pinned(&store, &keys(), &pin(1), mode)).unwrap();
        let kinds: Vec<_> = old.findings.iter().map(|f| f.kind).collect();
        assert_eq!(kinds, vec![FindingKind::NewerGenerationPresent], "{mode:?}");
        assert!(!old.has_tampering());
    }
}

#[test]
fn a_roll_forward_applies_generation_one_to_the_fork_then_generation_two_once_and_in_order() {
    let store = block_on(journal());
    let first = block_on(manifest_of(&store, 1));
    let second = block_on(manifest_of(&store, 2));
    let catalog = catalog(&[&first, &second]);

    let plan = plan(&catalog, None, Target::End).unwrap();
    let steps: Vec<_> = plan
        .steps
        .iter()
        .map(|s| (s.generation(), s.first_seq(), s.last_seq()))
        .collect();
    assert_eq!(steps, vec![(1, 1, 200), (2, 1, 10)]);
    assert_eq!(plan.ends_at, 10);
    assert!(plan.preserves_positions);
    assert!(plan.fork_point.is_none());

    let sink = replay(&store, &plan);
    let want: Vec<(u64, u64)> = (1..=200)
        .map(|seq| (1, seq))
        .chain((1..=10).map(|seq| (2, seq)))
        .collect();
    // Exactly positions 1..=200 of generation 1 and the new entries, once and in order; nothing of
    // the abandoned 201..=300.
    assert_eq!(applied(&sink), want);
    assert!(
        matches!(sink.calls().last(), Some(Call::Commit(p)) if *p == Position { generation: 2, seq: 10 })
    );
}

#[test]
fn a_point_in_time_rebuild_forks_a_third_generation_and_the_manifest_chain_holds_both_forks() {
    let store = block_on(journal());
    let first = block_on(manifest_of(&store, 1));
    let second = block_on(manifest_of(&store, 2));
    let catalog = catalog(&[&first, &second]);

    let planned = plan(&catalog, None, Target::At(5)).unwrap();
    let steps: Vec<_> = planned
        .steps
        .iter()
        .map(|s| (s.generation(), s.first_seq(), s.last_seq()))
        .collect();
    assert_eq!(steps, vec![(1, 1, 200), (2, 1, 5)]);
    let fork = planned.fork_point.unwrap();
    assert_eq!((fork.from_position, fork.abandoned_generation), (5, 2));
    assert_eq!(planned.new_generation, Some(3));
    assert!(!planned.preserves_positions);

    let sink = replay(&store, &planned);
    assert_eq!(applied(&sink).len(), 205);
    assert_eq!(applied(&sink).last(), Some(&(2, 5)));

    // The manifest of the new generation, forked as the plan says, holds both forks.
    let third = second
        .manifest()
        .fork(fork.from_position, "second rebuild", 10_000, 10_000)
        .unwrap();
    assert_eq!(Some(third.generation), planned.new_generation);
    let recorded: Vec<(u64, u64)> = third
        .forks
        .iter()
        .map(|f| (f.generation, f.forked_from_position))
        .collect();
    assert_eq!(recorded, vec![(2, 200), (3, 5)]);
    third.validate().unwrap();

    // A point in time inside an older generation cannot be recorded in that chain: refused.
    assert_eq!(
        plan_error(
            &catalog,
            Target::In {
                generation: 1,
                position: 150
            }
        ),
        PlanError::TargetBeforeFork {
            generation: 1,
            position: 150,
            current_generation: 2
        }
    );
    // And the abandoned tail of generation 1 is still named as abandoned.
    assert_eq!(
        plan_error(
            &catalog,
            Target::In {
                generation: 1,
                position: 250
            }
        ),
        PlanError::TargetInsideAbandonedGeneration {
            generation: 1,
            position: 250
        }
    );
}

fn plan_error(catalog: &JournalCatalog, target: Target) -> PlanError {
    plan(catalog, None, target).unwrap_err()
}

#[test]
fn a_verifier_given_only_the_first_manifest_and_pinned_to_the_second_refuses() {
    let full = block_on(journal());
    let only_first = SharedStore::default();
    let prefix = format!("{TENANT}/");
    for entry in block_on(full.list(&prefix, None, 1_000)).unwrap() {
        let keep = entry.key.contains("/manifest/00000000000000000001")
            || entry.key.contains("/segments/00000000000000000001/");
        if keep {
            let object = block_on(full.get(&entry.key)).unwrap().unwrap();
            block_on(only_first.put_new(&entry.key, object.bytes)).unwrap();
        }
    }
    let report = block_on(verify_pinned(&only_first, &keys(), &pin(2), Mode::Deep)).unwrap();
    assert!(!report.is_clean());
    assert!(report.has_tampering(), "a missing manifest is a rollback");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::MissingManifest)
    );
}
