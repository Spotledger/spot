//! Journal capture: one `doc_state` event per document per committed transaction (WP-JRN-02.T2;
//! ADR-061 section 3 and its 2026-10-31 amendment).
//!
//! The engine is the platform's only record write path, so the journal is complete if the engine
//! captures. Every write of every operation passes the write stage, which tells the
//! transaction's [`CaptureSet`] what it did. Just before the commit, [`Engine::flush_capture`]
//! turns the set into events, in the transaction, so a write never commits without its state
//! and a state never commits without its write.
//!
//! # What is captured
//!
//! For each `(DocType, id)` the transaction wrote, the **final** document as the last write left
//! it, and the **net** operation:
//!
//! | Writes in the transaction | Event |
//! | --- | --- |
//! | created, then (maybe) updated | one `insert` at the final version |
//! | updated, one or more times | one `update` at the final version |
//! | deleted (after any updates) | one `delete`, the image of the last version before the delete |
//! | created, then deleted | nothing (the change feed has no entry either) |
//!
//! Events are emitted in first-touch order, so the stream keeps the order in which the engine
//! first wrote the documents. A write rolled back to a savepoint (a failing step the caller
//! handled) is forgotten with the rest of that write's work.
//!
//! # The event
//!
//! The envelope's `subject` is the DocType and id, its `subject_version` the version storage
//! assigned. `data` is `{op, entity_schema, image}` (`spot_events_contract::state`):
//!
//! - `entity_schema` is the version of the metadata the operation ran on;
//! - `image` is the whole document in the native value encoding (ADR-055): `id`, `version`,
//!   `_state` when the DocType has one, every declared field (`null` when unset), and every child
//!   table as an array of rows, each with its `id`, its 1-based `idx` and its fields. Decimals are
//!   strings; instants are RFC 3339 in UTC.
//!
//! # What the image is built from
//!
//! The document **as storage holds it**, never filtered by the caller's field permissions: the
//! journal is the tenant's recoverable record, and permission filtering happens at the pull API
//! (ADR-061 section 7). The event is therefore in the audit-grade outbox, and only consumers
//! holding the explicit `doc_` stream right read it (`spot-event-stream`).
//!
//! **Secrets.** A `secret` field is not part of a [`Document`]: its sealed bytes live in
//! engine-managed columns that never reach the document, so the image carries no secret field at
//! all, sealed or not. A plaintext secret is never decrypted for the journal.
//!
//! **Personal data.** Personal fields stay in clear form inside the image **here**. The relay's
//! `ProtectTransform` seals them on the way out of the outbox (WP-RTB-01.T3 and T8).
//!
//! # Exclusions
//!
//! A [`CaptureConfig`] lists the DocTypes whose owner declared them outside the recoverable
//! record: derived data a projector rebuilds, or local-only data. Each carries an owner and a
//! reason code. [`Engine::with_capture`] refuses an exclusion without an owner or a reason
//! (`capture.exclusion_unowned`) and one naming a DocType the metadata does not have
//! (`capture.exclusion_unknown`). The list is exposed by [`Engine::capture_exclusions`], for the
//! recovery report and for the capture invariant's own exclusions.
//!
//! # Other writers
//!
//! Migrations and bulk loaders write through the engine, so this code captures them. Ledger and
//! stock entries written by Foundation will use the engine path or declare an exclusion with an
//! owner. The re-wrap sweep of the secret path writes records itself; it captures them with the
//! same event.
//!
//! # Failures
//!
//! No image, field value or id list appears in an error, log or metric. A failure to build or
//! enqueue an event fails the operation and rolls everything back.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Map, Value as Json, json};
use spot_app_api::context::Invocation;
use spot_app_api::error::{ErrorClass, StorageOp};
use spot_app_api::prelude::{Date, OffsetDateTime, RecordId, Time, Value, Version};
use spot_app_api::{AppError, AppResult, Document};
use spot_meta::{DOC_STATE, EffectiveMeta};
use spot_storage_contract::{Outbox, StorageBackend};
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;

use super::work::Work;
use super::{Engine, outbox};
use crate::lookup::resolve;
use crate::mapping::has_column;

/// The topic of a document's committed image.
pub const TOPIC_DOC_STATE: &str = "doc_state";

/// Why a DocType is outside the captured record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExclusionKind {
    /// Rebuilt by a projector after a rebuild (balances, caches, indexes, a mirror).
    Derived,
    /// Local-only by its sync policy; the owner states it is not part of the recoverable record.
    Local,
}

/// One declared exclusion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exclusion {
    /// The excluded DocType (storage name).
    pub doctype: String,
    /// Who excluded it. Only the owner of a DocType can.
    pub owner: String,
    /// Why, as a stable code.
    pub reason_code: String,
    /// Derived or local-only.
    pub kind: ExclusionKind,
}

/// What the engine captures. The default captures every DocType.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureConfig {
    /// The DocTypes that produce no `doc_state`, each with its owner and reason.
    pub exclude: Vec<Exclusion>,
}

fn refusal(code: &str, message: String) -> AppError {
    AppError::new(ErrorClass::Internal, code, message)
}

impl CaptureConfig {
    /// Checks the exclusions against `meta`.
    ///
    /// # Errors
    ///
    /// `capture.exclusion_unowned` for an exclusion with a blank owner or reason code,
    /// `capture.exclusion_unknown` for a DocType the metadata does not have (or a child
    /// DocType, which is never written alone), `capture.exclusion_duplicate` for a DocType
    /// listed twice.
    pub fn validate(&self, meta: &EffectiveMeta) -> AppResult<()> {
        let mut seen = BTreeSet::new();
        for exclusion in &self.exclude {
            if exclusion.owner.trim().is_empty() || exclusion.reason_code.trim().is_empty() {
                return Err(refusal(
                    "capture.exclusion_unowned",
                    format!(
                        "The capture exclusion of `{}` has no owner or no reason.",
                        exclusion.doctype
                    ),
                ));
            }
            let known = meta
                .doctype(&exclusion.doctype)
                .is_some_and(|def| !def.is_child);
            if !known {
                return Err(refusal(
                    "capture.exclusion_unknown",
                    format!(
                        "The capture exclusion names `{}`, which is not a DocType the catalogue has.",
                        exclusion.doctype
                    ),
                ));
            }
            if !seen.insert(exclusion.doctype.clone()) {
                return Err(refusal(
                    "capture.exclusion_duplicate",
                    format!(
                        "The DocType `{}` is excluded from capture twice.",
                        exclusion.doctype
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// The validated capture settings of an engine.
#[derive(Clone, Debug, Default)]
pub(crate) struct Capture {
    exclusions: Vec<Exclusion>,
    excluded: BTreeSet<String>,
}

impl Capture {
    pub(crate) fn new(config: CaptureConfig) -> Self {
        let excluded = config.exclude.iter().map(|e| e.doctype.clone()).collect();
        Self {
            exclusions: config.exclude,
            excluded,
        }
    }

    pub(crate) fn exclusions(&self) -> &[Exclusion] {
        &self.exclusions
    }

    pub(crate) fn captures(&self, doctype: &str) -> bool {
        !self.excluded.contains(doctype)
    }
}

/// What a transaction did to one document, taken as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Net {
    Insert,
    Update,
    Delete,
    /// Created and deleted in the same transaction: no event.
    Gone,
}

impl Net {
    fn op(self) -> Option<&'static str> {
        match self {
            Net::Insert => Some("insert"),
            Net::Update => Some("update"),
            Net::Delete => Some("delete"),
            Net::Gone => None,
        }
    }
}

#[derive(Clone)]
struct Entry {
    net: Net,
    /// The final document: as the last write left it; for a delete, as it was before.
    doc: Document,
}

/// The documents one transaction wrote, in first-touch order, with the net operation and final
/// image of each. It lives in the transaction's [`Work`] and is copied at a savepoint.
#[derive(Clone, Default)]
pub(crate) struct CaptureSet {
    entries: Vec<Entry>,
    index: HashMap<(String, RecordId), usize>,
}

impl CaptureSet {
    fn slot(&mut self, doc: &Document) -> Option<usize> {
        self.index
            .get(&(doc.doctype().as_str().to_owned(), doc.id()))
            .copied()
    }

    fn put(&mut self, at: Option<usize>, net: Net, doc: &Document) {
        match at {
            Some(i) => {
                self.entries[i] = Entry {
                    net,
                    doc: doc.clone(),
                }
            }
            None => {
                self.index.insert(
                    (doc.doctype().as_str().to_owned(), doc.id()),
                    self.entries.len(),
                );
                self.entries.push(Entry {
                    net,
                    doc: doc.clone(),
                });
            }
        }
    }

    /// A create or an update was written; `doc` is the document as saved (with its new version).
    pub(crate) fn wrote(&mut self, created: bool, doc: &Document) {
        let at = self.slot(doc);
        let net = match (at.map(|i| self.entries[i].net), created) {
            // Created in this transaction and written again: still an insert.
            (Some(Net::Insert), _) => Net::Insert,
            // Created after a deletion of the same id: the row exists before and after.
            (Some(Net::Delete), _) => Net::Update,
            (Some(Net::Gone), true) | (None, true) => Net::Insert,
            (Some(Net::Update | Net::Gone), false) | (None, false) | (Some(Net::Update), true) => {
                Net::Update
            }
        };
        self.put(at, net, doc);
    }

    /// A document was deleted; `doc` is the last image, as it was before the delete.
    pub(crate) fn deleted(&mut self, doc: &Document) {
        let at = self.slot(doc);
        let net = match at.map(|i| self.entries[i].net) {
            Some(Net::Insert | Net::Gone) => Net::Gone,
            Some(Net::Update | Net::Delete) | None => Net::Delete,
        };
        self.put(at, net, doc);
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn date_text(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

fn time_text(time: Time) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        time.hour(),
        time.minute(),
        time.second()
    )
}

fn instant_text(at: OffsetDateTime) -> String {
    let utc = at.to_offset(UtcOffset::UTC);
    utc.format(&Rfc3339)
        .unwrap_or_else(|_| utc.unix_timestamp().to_string())
}

/// One stored value in the native API value encoding (ADR-055): decimals as exact strings, JSON
/// fields embedded, bytes as base64, instants in UTC.
fn value_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => json!(i),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(t) => Json::String(t.clone()),
        Value::Id(id) => Json::String(id.to_string()),
        Value::Date(d) => Json::String(date_text(*d)),
        Value::Time(t) => Json::String(time_text(*t)),
        Value::DateTime(t) => Json::String(instant_text(*t)),
        Value::Json(text) => serde_json::from_str(text).unwrap_or(Json::Null),
        Value::Bytes(b) => Json::String(base64(b)),
        other => Json::String(format!("{other:?}")),
    }
}

/// The whole image of `doc`: every declared field and every child row, as storage holds it.
fn image(meta: &EffectiveMeta, doc: &Document) -> AppResult<Json> {
    let resolved = resolve(meta, doc.doctype().as_str())?;
    let mut map = Map::new();
    map.insert("id".to_owned(), Json::String(doc.id().to_string()));
    if let Some(version) = doc.version() {
        map.insert("version".to_owned(), json!(version.0));
    }
    if let Some(state) = doc.state() {
        map.insert("_state".to_owned(), Json::String(state.as_str().to_owned()));
    }
    for spec in resolved
        .def
        .fields
        .iter()
        .filter(|f| has_column(f) && f.name != DOC_STATE)
    {
        map.insert(spec.name.clone(), value_json(doc.get(&spec.name)));
    }
    for (table, child) in &resolved.tables {
        let mut rows = Vec::new();
        for (i, row) in doc.rows(&table.name).iter().enumerate() {
            let mut out = Map::new();
            out.insert("id".to_owned(), Json::String(row.id.to_string()));
            out.insert("idx".to_owned(), json!(i + 1));
            for spec in child.fields.iter().filter(|f| has_column(f)) {
                let value = row
                    .fields
                    .iter()
                    .find(|(name, _)| name.as_str() == spec.name)
                    .map_or(Value::Null, |(_, v)| v.clone());
                out.insert(spec.name.clone(), value_json(&value));
            }
            rows.push(Json::Object(out));
        }
        map.insert(table.name.clone(), Json::Array(rows));
    }
    Ok(Json::Object(map))
}

/// The `doc_state` event of `doc`, written as `op` at `version`.
pub(crate) fn state_event(
    engine_ids: &dyn super::IdGenerator,
    inv: &Invocation,
    meta: &EffectiveMeta,
    op: &str,
    doc: &Document,
    version: Version,
) -> AppResult<spot_storage_contract::OutboxEvent> {
    let data = json!({
        "op": op,
        "entity_schema": meta.version().max(1),
        "image": image(meta, doc)?,
    });
    outbox::doc_state(engine_ids, inv, doc, version, data)
}

impl<B: StorageBackend> Engine<B> {
    /// Makes the engine capture as `config` says (the default is every DocType). The
    /// exclusions are checked against the engine's current metadata.
    ///
    /// # Errors
    ///
    /// `capture.exclusion_unowned`, `capture.exclusion_unknown`, `capture.exclusion_duplicate`;
    /// see [`CaptureConfig::validate`].
    pub fn with_capture(mut self, config: CaptureConfig) -> AppResult<Self> {
        config.validate(&self.snapshot().meta)?;
        self.capture = Capture::new(config);
        Ok(self)
    }

    /// The DocTypes excluded from capture, each with its owner and reason, for the recovery
    /// report and the capture invariant's exclusions.
    pub fn capture_exclusions(&self) -> &[Exclusion] {
        self.capture.exclusions()
    }

    /// Records a create or update the write stage made.
    pub(crate) fn capture_wrote(&self, work: &mut Work<B::Tx>, created: bool, doc: &Document) {
        if self.capture.captures(doc.doctype().as_str()) {
            work.capture.wrote(created, doc);
        }
    }

    /// Records a delete the write stage made; `doc` is the last image.
    pub(crate) fn capture_deleted(&self, work: &mut Work<B::Tx>, doc: &Document) {
        if self.capture.captures(doc.doctype().as_str()) {
            work.capture.deleted(doc);
        }
    }

    /// Writes the `doc_state` events of the transaction, once, after its last write and before
    /// the commit. An error fails the operation: the caller rolls the transaction back.
    pub(crate) async fn flush_capture(
        &self,
        snap: &super::Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
    ) -> AppResult<()> {
        let entries = std::mem::take(&mut work.capture.entries);
        work.capture.index.clear();
        for entry in entries {
            let Some(op) = entry.net.op() else {
                continue;
            };
            let version = entry.doc.version().ok_or_else(|| {
                refusal(
                    "capture.event_failed",
                    "A written document has no version.".to_owned(),
                )
            })?;
            let event = state_event(&*self.ids, inv, &snap.meta, op, &entry.doc, version)?;
            work.tx
                .enqueue(&event)
                .await
                .map_err(|e| work.fail(e, StorageOp::Write))?;
        }
        Ok(())
    }
}
