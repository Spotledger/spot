//! A journal built from a script, and the live side that matches it.

use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use spot_events_contract::envelope::Position;
use spot_journal_manifest::{Manifest, SegmentSummary, VerifyingKeys, append_segments, sign};
use spot_journal_rebuild::RebuildKeys;
use spot_journal_rebuild_plan::{SegmentRange, SnapshotInfo};
use spot_journal_segment::{SealedSegment, SegmentConfig, SegmentWriter};
use spot_journal_snapshot::{
    ImageRecord, SnapshotKeys, SnapshotParams, SnapshotSigner, SnapshotWriter,
};
use spot_journal_store::{JournalStore, Layout, MemoryStore, TenantId};

use super::block_on;
use crate::AuditOptions;
use crate::memory::MemoryLive;

pub(crate) const TENANT: &str = "acme";
pub(crate) const SOURCE: &str = "acme.example";
const KEY_ID: &str = "journal-key-1";

/// A record id in canonical UUID form.
pub(crate) fn uuid(n: u64) -> String {
    format!("01900000-0000-7000-8000-{n:012x}")
}

/// One scripted state event; its position is its index plus one.
#[derive(Clone, Debug)]
pub(crate) enum Ev {
    Put {
        entity: &'static str,
        n: u64,
        version: u64,
        body: Value,
    },
    Delete {
        entity: &'static str,
        n: u64,
        version: u64,
    },
}

pub(crate) fn put(n: u64, version: u64) -> Ev {
    Ev::Put {
        entity: "account",
        n,
        version,
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

fn line(event: &Ev, position: Position, source: &str) -> String {
    let (entity, id, version, data) = match event {
        Ev::Put {
            entity,
            n,
            version,
            body,
        } => (
            *entity,
            *n,
            *version,
            json!({"op": "insert", "entity_schema": 1, "image": body}),
        ),
        Ev::Delete { entity, n, version } => (
            *entity,
            *n,
            *version,
            json!({"op": "delete", "entity_schema": 1, "image": {"gone": true}}),
        ),
    };
    json!({
        "event_id": uuid(1_000_000 + position.seq),
        "topic": "doc_state",
        "schema_version": 1,
        "source_id": source,
        "position": {"generation": position.generation, "seq": position.seq},
        "transaction": uuid(2_000_000 + position.seq),
        "ordinal": 0,
        "subject": {"entity": entity, "id": uuid(id)},
        "subject_version": version,
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": {"kind": "system", "component": "scheduler"},
            "correlation_id": uuid(3_000_000 + position.seq),
        },
        "data": data,
    })
    .to_string()
}

type Model = BTreeMap<(String, String), (u64, Value)>;

fn model(events: &[Ev], upto: usize) -> Model {
    let mut out = Model::new();
    for event in &events[..upto] {
        match event {
            Ev::Put {
                entity,
                n,
                version,
                body,
            } => {
                out.insert((entity.to_string(), uuid(*n)), (*version, body.clone()));
            }
            Ev::Delete { entity, n, .. } => {
                out.remove(&(entity.to_string(), uuid(*n)));
            }
        }
    }
    out
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[1; 32])
}

/// A test double for the snapshot signer and keys: the "signature" is the message reversed.
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

fn seal(events: &[Ev], per: usize) -> Vec<SealedSegment> {
    let mut sealed: Vec<SealedSegment> = Vec::new();
    for (chunk_index, chunk) in events.chunks(per).enumerate() {
        let mut config = match sealed.last() {
            Some(previous) => SegmentConfig::after(SOURCE, 1, previous.hash),
            None => SegmentConfig::first(SOURCE, 1),
        };
        config.block_target = 400;
        let mut writer = SegmentWriter::new(config).unwrap();
        for (offset, event) in chunk.iter().enumerate() {
            let seq = 1 + (chunk_index * per + offset) as u64;
            let position = Position { generation: 1, seq };
            writer
                .push(position, line(event, position, SOURCE).as_bytes())
                .unwrap();
        }
        sealed.push(writer.seal(["v1"]).unwrap());
    }
    sealed
}

fn summary(segment: &SealedSegment) -> SegmentSummary {
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

/// A journal of generation 1 for `TENANT` in a memory store.
pub(crate) struct Journal {
    pub(crate) store: MemoryStore,
    pub(crate) events: Vec<Ev>,
    pub(crate) segments: Vec<SealedSegment>,
    manifest_keys: VerifyingKeys,
}

impl Journal {
    pub(crate) fn new(events: Vec<Ev>, per: usize) -> Self {
        let segments = seal(&events, per);
        let summaries: Vec<SegmentSummary> = segments.iter().map(summary).collect();
        let manifest =
            append_segments(&Manifest::first(SOURCE, 1_000).unwrap(), &summaries).unwrap();
        let signed = sign(&manifest, &signing_key(), KEY_ID);
        let store = MemoryStore::new();
        block_on(store.put_new(
            &Layout::manifest(TENANT, 1).unwrap().to_key(),
            signed.to_bytes(),
        ))
        .unwrap();
        for segment in &segments {
            let h = &segment.header;
            let key = Layout::segment(TENANT, h.generation, h.first_seq, h.last_seq)
                .unwrap()
                .to_key();
            block_on(store.put_new(&key, segment.bytes.clone())).unwrap();
        }
        let mut manifest_keys = VerifyingKeys::new();
        manifest_keys
            .add_key(KEY_ID, signing_key().verifying_key(), 1, u64::MAX)
            .unwrap();
        Self {
            store,
            events,
            segments,
            manifest_keys,
        }
    }

    /// Writes a snapshot of the state after `upto` events, at entity schema `schema`. It carries
    /// no verified marker until something writes one.
    pub(crate) fn snapshot(&self, upto: u64, schema: u64) -> SnapshotInfo {
        let state = model(&self.events, usize::try_from(upto).unwrap());
        block_on(async {
            let params = SnapshotParams {
                tenant: TENANT.to_owned(),
                generation: 1,
                position: upto,
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
            writer.finish().await.unwrap()
        })
    }

    pub(crate) fn keys(&self) -> RebuildKeys<'_> {
        RebuildKeys {
            manifest: &self.manifest_keys,
            snapshot: &Toy,
        }
    }

    pub(crate) fn ranges(&self) -> Vec<SegmentRange> {
        self.segments
            .iter()
            .map(|s| SegmentRange {
                generation: 1,
                first_seq: s.header.first_seq,
                last_seq: s.header.last_seq,
            })
            .collect()
    }

    pub(crate) fn options(&self, rto_target_millis: u64) -> AuditOptions<'static> {
        AuditOptions::new(
            TenantId::new(TENANT).unwrap(),
            SOURCE,
            1,
            self.ranges(),
            rto_target_millis,
        )
    }

    /// A live side that holds exactly what the whole script produced.
    pub(crate) fn live(&self) -> MemoryLive {
        let mut live = MemoryLive::new(TENANT);
        for (index, event) in self.events.iter().enumerate() {
            let seq = index as u64 + 1;
            match event {
                Ev::Put {
                    entity,
                    n,
                    version,
                    body,
                } => live.put(seq, entity, &uuid(*n), *version, body),
                Ev::Delete { entity, n, version } => {
                    live.remove(seq, entity, &uuid(*n), *version);
                }
            }
        }
        live
    }
}
