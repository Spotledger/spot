//! A journal built from a script.

use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use spot_events_contract::envelope::Position;
use spot_journal_manifest::{Manifest, SegmentSummary, VerifyingKeys, append_segments, sign};
use spot_journal_rebuild_plan::{JournalCatalog, SegmentRange, SnapshotInfo, Target, plan};
use spot_journal_segment::{SealedSegment, SegmentConfig, SegmentWriter};
use spot_journal_snapshot::{
    ImageRecord, SnapshotKeys, SnapshotParams, SnapshotSigner, SnapshotWriter,
};
use spot_journal_store::{JournalStore, Layout, MemoryStore, TenantId};

use super::block_on;
use crate::{RebuildKeys, RebuildOptions};

pub(crate) const TENANT: &str = "acme";
pub(crate) const SOURCE: &str = "acme.example";
pub(crate) const KEY_ID: &str = "journal-key-1";

/// A record id in canonical UUID form.
pub(crate) fn uuid(n: u64) -> String {
    format!("01900000-0000-7000-8000-{n:012x}")
}

/// One scripted event; the position is its index plus one.
#[derive(Clone, Debug)]
pub(crate) enum Ev {
    Insert {
        entity: &'static str,
        n: u64,
        version: u64,
        schema: u64,
        body: Value,
    },
    Update {
        entity: &'static str,
        n: u64,
        version: u64,
        schema: u64,
        body: Value,
    },
    Delete {
        entity: &'static str,
        n: u64,
        version: u64,
    },
    Domain,
    Audit,
}

pub(crate) fn insert(n: u64, version: u64) -> Ev {
    Ev::Insert {
        entity: "account",
        n,
        version,
        schema: 1,
        body: json!({"name": format!("a{n}v{version}")}),
    }
}

pub(crate) fn update(n: u64, version: u64) -> Ev {
    Ev::Update {
        entity: "account",
        n,
        version,
        schema: 1,
        body: json!({"name": format!("a{n}v{version}")}),
    }
}

pub(crate) fn delete(n: u64, version: u64) -> Ev {
    Ev::Delete {
        entity: "account",
        n,
        version,
    }
}

/// The envelope JSON line of `event` at `position`, from `source`.
pub(crate) fn line(event: &Ev, position: Position, source: &str) -> String {
    let (topic, entity, id, version, data) = match event {
        Ev::Insert {
            entity,
            n,
            version,
            schema,
            body,
        } => (
            "doc_state",
            *entity,
            *n,
            Some(*version),
            json!({"op": "insert", "entity_schema": schema, "image": body}),
        ),
        Ev::Update {
            entity,
            n,
            version,
            schema,
            body,
        } => (
            "doc_state",
            *entity,
            *n,
            Some(*version),
            json!({"op": "update", "entity_schema": schema, "image": body}),
        ),
        Ev::Delete { entity, n, version } => (
            "doc_state",
            *entity,
            *n,
            Some(*version),
            json!({"op": "delete", "entity_schema": 1, "image": {"gone": true}}),
        ),
        Ev::Domain => (
            "doc_posted",
            "account",
            9_000,
            None,
            json!({"note": "posted"}),
        ),
        Ev::Audit => (
            "sys_role_changed",
            "role",
            8_000,
            None,
            json!({"role": "r1"}),
        ),
    };
    let mut envelope = json!({
        "event_id": uuid(1_000_000 + position.seq),
        "topic": topic,
        "schema_version": 1,
        "source_id": source,
        "position": {"generation": position.generation, "seq": position.seq},
        "transaction": uuid(2_000_000 + position.seq),
        "ordinal": 0,
        "subject": {"entity": entity, "id": uuid(id)},
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": {"kind": "system", "component": "scheduler"},
            "correlation_id": uuid(3_000_000 + position.seq),
        },
        "data": data,
    });
    if let Some(version) = version {
        envelope["subject_version"] = json!(version);
    }
    envelope.to_string()
}

/// The model: what the database holds after `events[..upto]`.
pub(crate) type Model = BTreeMap<(String, String), (u64, Value)>;

pub(crate) fn model(events: &[Ev], upto: usize) -> Model {
    let mut out = Model::new();
    for event in &events[..upto] {
        match event {
            Ev::Insert {
                entity,
                n,
                version,
                body,
                ..
            }
            | Ev::Update {
                entity,
                n,
                version,
                body,
                ..
            } => {
                out.insert((entity.to_string(), uuid(*n)), (*version, body.clone()));
            }
            Ev::Delete { entity, n, .. } => {
                out.remove(&(entity.to_string(), uuid(*n)));
            }
            Ev::Domain | Ev::Audit => {}
        }
    }
    out
}

pub(crate) fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[1; 32])
}

pub(crate) fn manifest_keys() -> VerifyingKeys {
    let mut keys = VerifyingKeys::new();
    keys.add_key(KEY_ID, signing_key().verifying_key(), 1, u64::MAX)
        .unwrap();
    keys
}

/// A test double for the snapshot signer and keys: the "signature" is the message reversed. It
/// proves the reader checks the signature at all; real signing is an adapter elsewhere.
pub(crate) struct Toy;

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

/// Seals `events` (positions `first_seq..`) of `source` into segments of `per` entries, with
/// small blocks so a segment has several.
pub(crate) fn seal(
    events: &[Ev],
    source: &str,
    generation: u64,
    first_seq: u64,
    per: usize,
) -> Vec<SealedSegment> {
    let mut sealed: Vec<SealedSegment> = Vec::new();
    for (chunk_index, chunk) in events.chunks(per).enumerate() {
        let mut config = match sealed.last() {
            Some(previous) => SegmentConfig::after(source, generation, previous.hash),
            None => SegmentConfig::first(source, generation),
        };
        config.block_target = 400;
        let mut writer = SegmentWriter::new(config).unwrap();
        for (offset, event) in chunk.iter().enumerate() {
            let seq = first_seq + (chunk_index * per + offset) as u64;
            let position = Position { generation, seq };
            writer
                .push(position, line(event, position, source).as_bytes())
                .unwrap();
        }
        sealed.push(writer.seal(["v1"]).unwrap());
    }
    sealed
}

pub(crate) fn summary(segment: &SealedSegment) -> SegmentSummary {
    let header = &segment.header;
    SegmentSummary::new(
        header.generation,
        header.first_seq,
        header.last_seq,
        header.entry_count,
        segment.hash,
        header.prev_hash,
    )
}

/// A journal of generation 1 in a store, for `tenant`.
pub(crate) struct Journal {
    pub(crate) store: MemoryStore,
    pub(crate) events: Vec<Ev>,
    pub(crate) segments: Vec<SealedSegment>,
    pub(crate) keys: VerifyingKeys,
}

pub(crate) fn segment_key(tenant: &str, segment: &SealedSegment) -> String {
    let h = &segment.header;
    Layout::segment(tenant, h.generation, h.first_seq, h.last_seq)
        .unwrap()
        .to_key()
}

impl Journal {
    /// A journal of `events` in segments of `per` entries, source `SOURCE`, tenant `TENANT`.
    pub(crate) fn new(events: Vec<Ev>, per: usize) -> Self {
        Self::build(TENANT, SOURCE, events, per)
    }

    pub(crate) fn build(tenant: &str, source: &str, events: Vec<Ev>, per: usize) -> Self {
        let segments = seal(&events, source, 1, 1, per);
        let summaries: Vec<SegmentSummary> = segments.iter().map(summary).collect();
        let manifest =
            append_segments(&Manifest::first(source, 1_000).unwrap(), &summaries).unwrap();
        let signed = sign(&manifest, &signing_key(), KEY_ID);
        let store = MemoryStore::new();
        block_on(store.put_new(
            &Layout::manifest(tenant, 1).unwrap().to_key(),
            signed.to_bytes(),
        ))
        .unwrap();
        for segment in &segments {
            block_on(store.put_new(&segment_key(tenant, segment), segment.bytes.clone())).unwrap();
        }
        Self {
            store,
            events,
            segments,
            keys: manifest_keys(),
        }
    }

    /// Writes a snapshot of the model after `upto` events at `(generation 1, position upto)`.
    pub(crate) fn snapshot(&self, upto: u64) -> SnapshotInfo {
        self.snapshot_of(
            &model(&self.events, usize::try_from(upto).unwrap()),
            upto,
            1,
        )
    }

    pub(crate) fn snapshot_of(&self, state: &Model, position: u64, schema: u64) -> SnapshotInfo {
        block_on(async {
            let params = SnapshotParams {
                tenant: TENANT.to_owned(),
                generation: 1,
                position,
                manifest_hash: "ab".repeat(32),
                taken_at_millis: 1_700_000_000_000,
            };
            let mut writer = SnapshotWriter::new(&self.store, params, &Toy)
                .and_then(|w| w.with_part_bytes(300))
                .unwrap();
            writer
                .write_part(
                    state
                        .iter()
                        .map(|((entity, id), (version, body))| ImageRecord {
                            entity: entity.clone(),
                            id: id.clone(),
                            version: *version,
                            entity_schema: schema,
                            image: body.clone(),
                        }),
                )
                .await
                .unwrap();
            let mut info = writer.finish().await.unwrap();
            info.verified = true;
            info
        })
    }

    /// The catalogue of what this journal holds, with `snapshots`.
    pub(crate) fn catalog(&self, snapshots: Vec<SnapshotInfo>) -> JournalCatalog {
        JournalCatalog {
            snapshots,
            segments: self
                .segments
                .iter()
                .map(|s| SegmentRange {
                    generation: 1,
                    first_seq: s.header.first_seq,
                    last_seq: s.header.last_seq,
                })
                .collect(),
            forks: Vec::new(),
        }
    }

    pub(crate) fn plan(
        &self,
        snapshots: Vec<SnapshotInfo>,
        target: Target,
    ) -> spot_journal_rebuild_plan::RebuildPlan {
        plan(&self.catalog(snapshots), None, target).unwrap()
    }

    pub(crate) fn keys(&self) -> RebuildKeys<'_> {
        RebuildKeys {
            manifest: &self.keys,
            snapshot: &Toy,
        }
    }
}

pub(crate) fn options(target_schema: u64) -> RebuildOptions<'static> {
    RebuildOptions::new(TenantId::new(TENANT).unwrap(), SOURCE, target_schema)
}
