//! A journal for one tenant, and the pieces of a `TenantInputs` built from it.

use std::rc::Rc;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use spot_conditions::{ConditionEngine, MemorySink, Registry};
use spot_events_contract::envelope::Position;
use spot_image_upcast::Registry as UpcastRegistry;
use spot_journal_audit_run::AuditOptions;
use spot_journal_audit_run::memory::{MemoryLive, MemoryScratch};
use spot_journal_manifest::{Manifest, SegmentSummary, VerifyingKeys, append_segments, sign};
use spot_journal_rebuild_plan::SegmentRange;
use spot_journal_segment::{SealedSegment, SegmentConfig, SegmentWriter};
use spot_journal_snapshot::SnapshotKeys;
use spot_journal_store::{JournalStore, Layout, MemoryStore, TenantId};

use crate::{TenantInputs, register_conditions};

pub(crate) const SOURCE: &str = "acme.example";
const KEY_ID: &str = "journal-key-1";

/// A record id in canonical UUID form.
pub(crate) fn uuid(n: u64) -> String {
    format!("01900000-0000-7000-8000-{n:012x}")
}

/// A snapshot key set that trusts nothing: the tests use no snapshot.
struct NoSnapshots;

impl SnapshotKeys for NoSnapshots {
    fn verify(&self, _: &str, _: u64, _: &[u8], _: &[u8]) -> bool {
        false
    }
}

fn line(n: u64, version: u64, body: &Value, position: Position) -> String {
    json!({
        "event_id": uuid(1_000_000 + position.seq),
        "topic": "doc_state",
        "schema_version": 1,
        "source_id": SOURCE,
        "position": {"generation": position.generation, "seq": position.seq},
        "transaction": uuid(2_000_000 + position.seq),
        "ordinal": 0,
        "subject": {"entity": "account", "id": uuid(n)},
        "subject_version": version,
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": {"kind": "system", "component": "scheduler"},
            "correlation_id": uuid(3_000_000 + position.seq),
        },
        "data": {"op": "insert", "entity_schema": 1, "image": body},
    })
    .to_string()
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[1; 32])
}

/// A journal of generation 1 for one tenant: `records` accounts, one per position, two per
/// segment.
pub(crate) struct Journal {
    pub(crate) tenant: String,
    pub(crate) store: Arc<MemoryStore>,
    records: u64,
    segments: Vec<SealedSegment>,
    keys: Arc<VerifyingKeys>,
}

fn body(n: u64) -> Value {
    json!({"name": format!("a{n}")})
}

impl Journal {
    pub(crate) fn new(rt: &tokio::runtime::Runtime, tenant: &str, records: u64) -> Self {
        let mut segments: Vec<SealedSegment> = Vec::new();
        let all: Vec<u64> = (1..=records).collect();
        for (chunk_index, chunk) in all.chunks(2).enumerate() {
            let mut config = match segments.last() {
                Some(previous) => SegmentConfig::after(SOURCE, 1, previous.hash),
                None => SegmentConfig::first(SOURCE, 1),
            };
            config.block_target = 400;
            let mut writer = SegmentWriter::new(config).unwrap();
            for (offset, n) in chunk.iter().enumerate() {
                let seq = 1 + (chunk_index * 2 + offset) as u64;
                let position = Position { generation: 1, seq };
                writer
                    .push(position, line(*n, 1, &body(*n), position).as_bytes())
                    .unwrap();
            }
            segments.push(writer.seal(["v1"]).unwrap());
        }
        let summaries: Vec<SegmentSummary> = segments
            .iter()
            .map(|s| {
                let h = &s.header;
                SegmentSummary::new(
                    h.generation,
                    h.first_seq,
                    h.last_seq,
                    h.entry_count,
                    s.hash,
                    h.prev_hash,
                )
            })
            .collect();
        let manifest =
            append_segments(&Manifest::first(SOURCE, 1_000).unwrap(), &summaries).unwrap();
        let signed = sign(&manifest, &signing_key(), KEY_ID);
        let store = Arc::new(MemoryStore::new());
        rt.block_on(store.put_new(
            &Layout::manifest(tenant, 1).unwrap().to_key(),
            signed.to_bytes(),
        ))
        .unwrap();
        for segment in &segments {
            let h = &segment.header;
            let key = Layout::segment(tenant, h.generation, h.first_seq, h.last_seq)
                .unwrap()
                .to_key();
            rt.block_on(store.put_new(&key, segment.bytes.clone()))
                .unwrap();
        }
        let mut keys = VerifyingKeys::new();
        keys.add_key(KEY_ID, signing_key().verifying_key(), 1, u64::MAX)
            .unwrap();
        Self {
            tenant: tenant.to_owned(),
            store,
            records,
            segments,
            keys: Arc::new(keys),
        }
    }

    pub(crate) fn live(&self) -> MemoryLive {
        let mut live = MemoryLive::new(self.tenant.as_str());
        for n in 1..=self.records {
            live.put(n, "account", &uuid(n), 1, &body(n));
        }
        live
    }

    pub(crate) fn tampered_live(&self) -> MemoryLive {
        let mut live = self.live();
        live.put(
            2,
            "account",
            &uuid(2),
            1,
            &json!({"name": "SECRET-TAMPERED"}),
        );
        live.set_head(spot_journal_audit::Position::new(1, self.records));
        live
    }

    fn options(&self, truncate: bool) -> AuditOptions<'static> {
        let mut segments: Vec<SegmentRange> = self
            .segments
            .iter()
            .map(|s| SegmentRange {
                generation: 1,
                first_seq: s.header.first_seq,
                last_seq: s.header.last_seq,
            })
            .collect();
        if truncate {
            segments.pop();
        }
        AuditOptions::new(
            TenantId::new(&self.tenant).unwrap(),
            SOURCE,
            1,
            segments,
            1_000,
        )
    }

    /// Inputs with the given live side; `truncate` drops the last segment from the plan, so the
    /// rebuild cannot reach the live head.
    pub(crate) fn inputs(&self, live: MemoryLive, truncate: bool) -> TenantInputs {
        TenantInputs {
            options: self.options(truncate),
            live: Box::new(live),
            scratch: Box::new(MemoryScratch::new(self.tenant.as_str())),
            store: Arc::clone(&self.store) as Arc<dyn JournalStore>,
            manifest_keys: Arc::clone(&self.keys),
            snapshot_keys: Arc::new(NoSnapshots),
            registry: Rc::new(UpcastRegistry::new()),
        }
    }
}

/// The engine as the host builds it: the catalogue, then this crate's own definition.
pub(crate) fn engine() -> ConditionEngine<MemorySink> {
    let mut registry = spot_sync_conditions::catalogue().unwrap();
    register_conditions(&mut registry).unwrap();
    ConditionEngine::new(registry, MemorySink::new())
}

/// An engine whose registry has only this crate's definition: the host forgot the catalogue.
pub(crate) fn engine_without_catalogue() -> ConditionEngine<MemorySink> {
    let mut registry = Registry::new();
    register_conditions(&mut registry).unwrap();
    ConditionEngine::new(registry, MemorySink::new())
}

pub(crate) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
