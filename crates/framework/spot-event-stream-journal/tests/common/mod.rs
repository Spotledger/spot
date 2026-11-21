//! Test support: a real signed journal in a memory store, envelopes, a text tail codec.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use serde_json::json;
use spot_event_stream_journal::JournalReader;
use spot_events_contract::envelope::{EventEnvelope, Position};
use spot_events_contract::names::{CommandId, SourceId, Topic};
use spot_events_contract::payload::{Actor, EventMeta, PayloadDocument};
use spot_journal_manifest::{Manifest, SegmentSummary, VerifyingKeys, sign};
use spot_journal_rebuild_plan::unsealed::{ChunkFault, TailChunkReader, TailEntry, TailPosition};
use spot_journal_segment::{SegmentConfig, SegmentWriter, genesis_hash};
use spot_journal_store::{JournalStore, Layout, MemoryStore, TailKey};
use spot_storage_contract::{FeedPosition, Name, NodeRef, OutboxRecord, RecordId};
use time::macros::datetime;

pub const TENANT: &str = "acme";
pub const SOURCE: &str = "acme-site";
const KEY_ID: &str = "journal-key-1";

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

pub fn keys() -> Arc<VerifyingKeys> {
    let mut keys = VerifyingKeys::new();
    keys.add_key(KEY_ID, signing_key().verifying_key(), 1, u64::MAX)
        .expect("key window");
    Arc::new(keys)
}

fn id(n: u64) -> RecordId {
    let mut b = [0u8; 16];
    b[0] = 0x01;
    b[6] = 0x70;
    b[8] = 0x80;
    b[10..16].copy_from_slice(&n.to_be_bytes()[2..8]);
    RecordId::from_bytes(b).expect("record id")
}

/// An envelope at `(generation, seq)`; `kind` goes into the payload so a selector can pick.
pub fn envelope(generation: u64, seq: u64, kind: &str) -> EventEnvelope {
    let doc = PayloadDocument {
        meta: EventMeta {
            occurred_at: datetime!(2026-09-29 15:00:00 +05:00),
            actor: Actor::User {
                id: id(1),
                via: None,
            },
            command_id: Some(CommandId::new("req-1").expect("command id")),
            causation_id: None,
            correlation_id: id(9),
            trace_id: None,
        },
        data: json!({"kind": kind, "seq": seq}),
    };
    let subject = NodeRef {
        entity: Name::new("sales_invoice").expect("name"),
        id: id(42),
    };
    let event = doc
        .to_outbox_event(
            id(generation * 1_000_000 + seq),
            &Topic::app("test_event").expect("topic"),
            subject,
            None,
            1,
        )
        .expect("event");
    let record = OutboxRecord {
        position: FeedPosition::new(generation, seq),
        transaction: id(5),
        ordinal: 0,
        event,
    };
    let source = SourceId::new("acme.spotledger.app").expect("source");
    EventEnvelope::from_record(&source, &record).expect("envelope")
}

pub fn line(generation: u64, seq: u64) -> String {
    envelope(generation, seq, "plain").to_json().expect("json")
}

pub fn pos(generation: u64, seq: u64) -> Position {
    Position { generation, seq }
}

/// One generation being built: its segments (as `(first, last)` ranges) and its manifest.
pub struct Built {
    pub manifest: Manifest,
    pub segments: Vec<(SegmentSummary, Vec<u8>)>,
}

/// Seals `ranges` of `generation` into chained segments and a manifest that lists them.
/// `base` is the manifest to grow (`Manifest::first` for generation 1, a fork result later).
pub fn build(base: Manifest, ranges: &[(u64, u64)]) -> Built {
    let generation = base.generation;
    let mut prev = genesis_hash(SOURCE);
    let mut summaries = Vec::new();
    let mut segments = Vec::new();
    for (first, last) in ranges {
        let mut writer =
            SegmentWriter::new(SegmentConfig::after(SOURCE, generation, prev)).expect("writer");
        for seq in *first..=*last {
            writer
                .push(pos(generation, seq), line(generation, seq).as_bytes())
                .expect("push");
        }
        let sealed = writer.seal(["1"]).expect("seal");
        let summary = SegmentSummary::new(
            generation,
            *first,
            *last,
            last - first + 1,
            sealed.hash,
            prev,
        );
        prev = sealed.hash;
        summaries.push(summary);
        segments.push((summary, sealed.bytes));
    }
    let manifest = base.append_segments(&summaries).expect("append");
    Built { manifest, segments }
}

impl Built {
    /// A copy whose `index`-th stored segment has one bit flipped (the manifest is unchanged).
    pub fn clone_for_tamper(&self) -> Tampered {
        Tampered {
            built: Built {
                manifest: self.manifest.clone(),
                segments: self.segments.clone(),
            },
        }
    }
}

pub struct Tampered {
    pub built: Built,
}

impl Tampered {
    pub fn flip_bit_in_segment(&mut self, index: usize) {
        let bytes = &mut self.built.segments[index].1;
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x01;
    }
}

pub fn put_built(store: &MemoryStore, built: &Built) {
    for (summary, bytes) in &built.segments {
        let key = Layout::segment(
            TENANT,
            summary.generation,
            summary.first_seq,
            summary.last_seq,
        )
        .expect("key")
        .to_key();
        block(store.put_new(&key, bytes.clone())).expect("put segment");
    }
    put_manifest(store, &built.manifest);
}

pub fn put_manifest(store: &MemoryStore, manifest: &Manifest) {
    let signed = sign(manifest, &signing_key(), KEY_ID);
    let key = Layout::manifest(TENANT, manifest.generation)
        .expect("key")
        .to_key();
    block(store.put_new(&key, signed.to_bytes())).expect("put manifest");
}

/// A tail chunk in the test codec: `gen seq json` lines.
pub fn put_chunk(store: &MemoryStore, generation: u64, batch_id: u64, first: u64, last: u64) {
    let mut text = String::new();
    for seq in first..=last {
        text.push_str(&format!("{generation} {seq} {}\n", line(generation, seq)));
    }
    let key = Layout::tail_chunk(TENANT, generation, batch_id)
        .expect("key")
        .to_key();
    block(store.put_new(&key, text.into_bytes())).expect("put chunk");
}

pub struct TextCodec;

impl TailChunkReader for TextCodec {
    fn decode(
        &self,
        _key: &TailKey,
        _object_key: &str,
        bytes: &[u8],
    ) -> Result<Vec<TailEntry>, ChunkFault> {
        let text = std::str::from_utf8(bytes).map_err(|_| ChunkFault { letter: 'c' })?;
        let mut out = Vec::new();
        for row in text.lines() {
            let mut fields = row.splitn(3, ' ');
            let generation = fields.next().and_then(|v| v.parse().ok());
            let seq = fields.next().and_then(|v| v.parse().ok());
            let body = fields.next();
            let (Some(generation), Some(seq), Some(body)) = (generation, seq, body) else {
                return Err(ChunkFault { letter: 'c' });
            };
            out.push(TailEntry {
                position: TailPosition { generation, seq },
                line: body.as_bytes().to_vec(),
            });
        }
        Ok(out)
    }
}

pub fn reader(store: &Arc<MemoryStore>) -> JournalReader<MemoryStore> {
    JournalReader::new(Arc::clone(store), TENANT, keys(), Arc::new(TextCodec)).expect("reader")
}

pub fn block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(future)
}

pub fn seqs(page: &spot_event_stream_journal::HistoryPage) -> Vec<(u64, u64)> {
    page.envelopes
        .iter()
        .map(|e| (e.position.generation, e.position.seq))
        .collect()
}
