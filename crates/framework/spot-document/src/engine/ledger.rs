//! The per-site command ledger of the document engine (WP-DOC-04.T8; ADR-055, ADR-056).
//!
//! A repeated command id (after a crash, a restart or a lost reply) is answered with the original
//! outcome and runs no participant, step, hook, write, outbox event or audit row a second time.
//!
//! # How it works
//!
//! For a top-level write (`create`, `update`, `post`, `cancel`, `correct`, `delete`, `rename`;
//! never a read, a nested write, a method call or a secret write) whose [`Invocation`] has a
//! command id, the engine's attempt first asks the transaction for the record of
//! `(actor principal, command id)`:
//!
//! * **absent**: the operation runs as before and, as the last statement before the commit, the
//!   record is written **in the same transaction**, so the ledger row exists if and only if the
//!   document change does;
//! * **present with the same fingerprint**: the transaction is rolled back without a write and
//!   the stored outcome is answered ([`Engine::replayed`]); the document is loaded now, under the
//!   caller's own read permission, so a field the caller may not read is masked;
//! * **present with another fingerprint**: `document.command_conflict`, nothing written.
//!
//! A failed operation records nothing (its transaction rolls back), so the same id may be tried
//! again after a fix. A `CommitOutcomeUnknown` leaves no knowledge: the retry reads the ledger, and
//! the row is there exactly when the commit happened. Two concurrent first uses of one key: the
//! storage key rejects the second insert, its transaction rolls back, and the engine then looks
//! the key up again and replays. No in-process lock stands in for the storage key.
//!
//! # Fingerprint
//!
//! SHA-256 over a tagged, length-prefixed encoding of the operation, the DocType, the document id
//! (or the create payload), the `If-Match` version and the patch or values. Values are encoded
//! type by type (the encoding never depends on a `Debug` format, except for the two geo and range
//! types, which have no accessors yet). The digest is all that is stored; no value is.
//!
//! # The stored outcome
//!
//! Small, with no document body and no secret: the operation, the DocType, the document id, the
//! resulting version, and the operation's messages. At most 16 KiB (the storage contract's limit).
//!
//! # Retention
//!
//! Retention is configuration, per caller class, recorded per row ([`RetentionPolicy`]): the
//! engine writes the row's own `retain_until` at write time, so changing the setting later never
//! shortens or extends a row that exists, and [`Engine::prune_commands`] deletes by `retain_until`
//! only. The library never reads the wall clock: the writer's time comes from the engine's
//! `Clock`, and `prune_commands` takes `now` as an argument.
//!
//! The ledger is **opt-in** ([`Engine::with_command_ledger`]): an engine without a policy behaves
//! as before. With a policy, a backend that answers `Unsupported` for the ledger methods makes the
//! operation fail (the storage error is mapped), never silently skip the ledger.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use spot_app_api::context::{DocumentService, Invocation, Outcome};
use spot_app_api::error::{StorageOp, map_storage_error};
use spot_app_api::prelude::{OffsetDateTime, RecordId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, DocTypeDef, Document, DocumentPatch, ErrorClass, FieldPath,
    Level, Message, Operation,
};
use spot_permissions::Action;
use spot_storage_contract::{CommandRecord, Isolation, StorageBackend, Transaction, TxOptions};
use std::sync::Arc;

use super::Engine;
use super::ops::{Done, Req, Ret, label, not_found};
use super::receipt::{Finished, ReceiptRecord};

/// Documented default of [`RetentionPolicy::default_duration`]: seven days. (24 hours, the HTTP
/// store's window, is too short for a BPM retry or an import resumed on Monday.)
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Documented default of [`RetentionPolicy::max_duration`]: thirty days, so an unconfigured site
/// is bounded.
pub const DEFAULT_MAX_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The version of the stored outcome's shape.
const OUTCOME_VERSION: u64 = 1;

/// How long a command ledger row is kept, per caller class (the architecture owner, 5 Nov 2026).
///
/// Plain data, no I/O. The host parses it from the site configuration
/// (`[idempotency.ledger.retention]`, WP-PLT-05.T9) and passes it to
/// [`Engine::with_command_ledger`]. A caller selects a configured class **by name only**
/// ([`Engine::for_class`]); an unknown or absent class means the default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    default: Duration,
    max: Duration,
    classes: BTreeMap<String, Duration>,
}

fn invalid(what: &str) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "document.internal_error",
        format!("The command ledger retention policy is not usable: {what}."),
    )
}

impl RetentionPolicy {
    /// A policy with the given default, cap and per-class durations.
    ///
    /// A class longer than `max` is clamped, not refused ([`clamped_classes`](Self::clamped_classes)
    /// reports them so the host can log it).
    ///
    /// # Errors
    ///
    /// `document.internal_error` for a zero `default`, a `max` below `default`, or a zero class.
    pub fn new(
        default: Duration,
        max: Duration,
        classes: BTreeMap<String, Duration>,
    ) -> AppResult<Self> {
        if default.is_zero() {
            return Err(invalid("the default retention is zero"));
        }
        if max < default {
            return Err(invalid("the maximum retention is below the default"));
        }
        if let Some((name, _)) = classes.iter().find(|(_, d)| d.is_zero()) {
            return Err(invalid(&format!("the class `{name}` has a zero retention")));
        }
        Ok(Self {
            default,
            max,
            classes,
        })
    }

    /// The default retention of a class that is unknown or not named.
    #[must_use]
    pub fn default_duration(&self) -> Duration {
        self.default
    }

    /// The cap no row is kept beyond.
    #[must_use]
    pub fn max_duration(&self) -> Duration {
        self.max
    }

    /// The classes whose configured duration is longer than the cap (they are clamped), in name
    /// order, for the host to log.
    #[must_use]
    pub fn clamped_classes(&self) -> Vec<&str> {
        self.classes
            .iter()
            .filter(|(_, d)| **d > self.max)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// When a row recorded at `recorded_at` by a caller of `class` may be pruned: the class's
    /// retention (the default for an unknown or absent class), clamped to the cap, added to
    /// `recorded_at`. Pure: it reads no clock. A row is pruned when `retain_until <= now`.
    #[must_use]
    pub fn retain_until(&self, class: Option<&str>, recorded_at: OffsetDateTime) -> OffsetDateTime {
        let wanted = class
            .and_then(|c| self.classes.get(c))
            .copied()
            .unwrap_or(self.default);
        let span = wanted.min(self.max);
        let seconds = i64::try_from(span.as_secs()).unwrap_or(i64::MAX);
        recorded_at.saturating_add(time::Duration::seconds(seconds))
    }
}

impl Default for RetentionPolicy {
    /// Seven days by default, thirty at most, no classes.
    fn default() -> Self {
        Self {
            default: DEFAULT_RETENTION,
            max: DEFAULT_MAX_RETENTION,
            classes: BTreeMap::new(),
        }
    }
}

/// What one operation needs of the ledger, computed once from the request.
pub(crate) struct LedgerCall {
    pub(crate) principal: String,
    pub(crate) command_id: String,
    pub(crate) fingerprint: Vec<u8>,
    pub(crate) class: Option<String>,
}

/// What one attempt produced: the operation ran, or the ledger answered it.
pub(crate) enum Attempted<T> {
    /// The operation ran and committed.
    Ran(T),
    /// The command id was recorded before: nothing ran.
    Replayed(CommandRecord),
}

pub(crate) fn command_conflict() -> AppError {
    AppError::new(
        ErrorClass::Duplicate,
        "document.command_conflict",
        "This command id was already used for a different request.",
    )
}

// ------------------------------------------------------------------------------------------
// Fingerprint
// ------------------------------------------------------------------------------------------

fn put(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}

fn put_value(h: &mut Sha256, value: &Value) {
    match value {
        Value::Null => h.update([0u8]),
        Value::Bool(b) => h.update([1u8, u8::from(*b)]),
        Value::Int(i) => {
            h.update([2u8]);
            h.update(i.to_be_bytes());
        }
        Value::Decimal(d) => {
            h.update([3u8]);
            put(h, d.normalize().to_string().as_bytes());
        }
        Value::Text(s) => {
            h.update([4u8]);
            put(h, s.as_bytes());
        }
        Value::Date(d) => {
            h.update([5u8]);
            put(h, d.to_string().as_bytes());
        }
        Value::Time(t) => {
            h.update([6u8]);
            put(h, t.to_string().as_bytes());
        }
        Value::DateTime(t) => {
            h.update([7u8]);
            h.update(t.unix_timestamp_nanos().to_be_bytes());
        }
        Value::Id(id) => {
            h.update([8u8]);
            h.update(id.as_bytes());
        }
        Value::Json(text) => {
            h.update([9u8]);
            // Backends may normalise whitespace and key order: hash the parsed value, whose maps
            // serialise in key order.
            let canonical =
                serde_json::from_str::<Json>(text).map_or_else(|_| text.clone(), |j| j.to_string());
            put(h, canonical.as_bytes());
        }
        Value::Bytes(bytes) => {
            h.update([10u8]);
            put(h, bytes);
        }
        other => {
            h.update([11u8]);
            put(h, format!("{other:?}").as_bytes());
        }
    }
}

fn put_fields(h: &mut Sha256, fields: &BTreeMap<spot_app_api::prelude::Name, Value>) {
    h.update((fields.len() as u64).to_be_bytes());
    for (name, value) in fields {
        put(h, name.as_str().as_bytes());
        put_value(h, value);
    }
}

fn put_id(h: &mut Sha256, id: RecordId) {
    h.update(id.as_bytes());
}

fn put_document(h: &mut Sha256, doc: &Document) {
    put(h, doc.doctype().as_str().as_bytes());
    put_id(h, doc.id());
    put_fields(h, doc.fields());
    h.update((doc.tables().len() as u64).to_be_bytes());
    for (table, rows) in doc.tables() {
        put(h, table.as_str().as_bytes());
        h.update((rows.len() as u64).to_be_bytes());
        for row in rows {
            put_id(h, row.id);
            put_fields(h, &row.fields);
        }
    }
}

fn put_patch(h: &mut Sha256, patch: &DocumentPatch) {
    put_fields(h, &patch.fields);
    h.update((patch.tables.len() as u64).to_be_bytes());
    for (table, rows) in &patch.tables {
        put(h, table.as_str().as_bytes());
        h.update((rows.len() as u64).to_be_bytes());
        for row in rows {
            match row.id {
                Some(id) => {
                    h.update([1u8]);
                    put_id(h, id);
                }
                None => h.update([0u8]),
            }
            put_fields(h, &row.fields);
        }
    }
}

/// The fingerprint of a request, or `None` for one the ledger does not cover.
pub(crate) fn fingerprint(req: &Req) -> Option<Vec<u8>> {
    let mut h = Sha256::new();
    h.update(b"spot.command.v1");
    put(&mut h, req.op().as_bytes());
    match req {
        Req::Create(doc) => put_document(&mut h, doc),
        Req::Update {
            doctype,
            id,
            patch,
            expected,
        } => {
            put(&mut h, doctype.as_bytes());
            put_id(&mut h, *id);
            h.update(expected.0.to_be_bytes());
            put_patch(&mut h, patch);
        }
        Req::Delete {
            doctype,
            id,
            expected,
        }
        | Req::Post {
            doctype,
            id,
            expected,
        }
        | Req::Cancel {
            doctype,
            id,
            expected,
        }
        | Req::Correct {
            doctype,
            id,
            expected,
        } => {
            put(&mut h, doctype.as_bytes());
            put_id(&mut h, *id);
            h.update(expected.0.to_be_bytes());
        }
        Req::Rename {
            doctype,
            id,
            new_name,
            expected,
        } => {
            put(&mut h, doctype.as_bytes());
            put_id(&mut h, *id);
            h.update(expected.0.to_be_bytes());
            put(&mut h, new_name.as_bytes());
        }
        // A method returns a result that may be a body, and a secret write carries sealed
        // values whose bytes change on every sealing: neither is recorded.
        Req::Method { .. } | Req::SetSecrets { .. } | Req::CreateWithSecrets { .. } => return None,
    }
    Some(h.finalize().to_vec())
}

// ------------------------------------------------------------------------------------------
// The stored outcome
// ------------------------------------------------------------------------------------------

fn hex(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        out.push(char::from(b"0123456789abcdef"[usize::from(byte & 0x0f)]));
    }
    out
}

fn unhex(text: &str) -> Option<RecordId> {
    if text.len() != 32 || !text.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    RecordId::from_bytes(bytes).ok()
}

fn level_text(level: Level) -> &'static str {
    match level {
        Level::Info => "info",
        Level::Warning => "warning",
    }
}

fn message_json(message: &Message) -> Json {
    json!({
        "level": level_text(message.level),
        "code": message.code,
        "text": message.text,
        "field": message.field.as_ref().map(|f| f.as_str().to_owned()),
    })
}

fn message_of(value: &Json) -> Option<Message> {
    let level = match value.get("level")?.as_str()? {
        "info" => Level::Info,
        "warning" => Level::Warning,
        _ => return None,
    };
    let code = value.get("code")?.as_str()?;
    let text = value.get("text")?.as_str()?;
    let mut message = match level {
        Level::Info => Message::info(code, text),
        Level::Warning => Message::warning(code, text),
    };
    if let Some(path) = value.get("field").and_then(Json::as_str)
        && let Some(path) = FieldPath::parse(path)
    {
        message = message.on(path);
    }
    Some(message)
}

/// What a replay is told: which operation ran on which document, to what version.
pub(crate) struct Stored {
    pub(crate) operation: String,
    pub(crate) doctype: String,
    pub(crate) id: RecordId,
    pub(crate) version: Option<Version>,
    pub(crate) messages: Vec<Message>,
}

impl Stored {
    fn parse(text: &str) -> AppResult<Self> {
        let bad = || {
            AppError::new(
                ErrorClass::Internal,
                "document.internal_error",
                "A stored command outcome could not be read.",
            )
        };
        let json: Json = serde_json::from_str(text).map_err(|_| bad())?;
        if json.get("v").and_then(Json::as_u64) != Some(OUTCOME_VERSION) {
            return Err(bad());
        }
        let text_of = |key: &str| json.get(key).and_then(Json::as_str).map(str::to_owned);
        Ok(Self {
            operation: text_of("operation").ok_or_else(bad)?,
            doctype: text_of("doctype").ok_or_else(bad)?,
            id: text_of("id").as_deref().and_then(unhex).ok_or_else(bad)?,
            version: json.get("version").and_then(Json::as_u64).map(Version),
            messages: json
                .get("messages")
                .and_then(Json::as_array)
                .map(|all| all.iter().filter_map(message_of).collect())
                .unwrap_or_default(),
        })
    }
}

fn operation_of(name: &str) -> Operation {
    match name {
        "create" => Operation::Create,
        "update" => Operation::Update,
        "post" => Operation::Post,
        "delete" => Operation::Delete,
        "rename" => Operation::Rename,
        // `correct` reports the cancel of the original as its own receipt.
        _ => Operation::Cancel,
    }
}

impl<B: StorageBackend> Engine<B> {
    /// The same engine recording every top-level write that carries a command id in a command
    /// ledger, and answering a repeat with the original outcome. `policy` says how long a row is
    /// kept, per caller class (see [`RetentionPolicy`]). Without this the engine keeps no ledger.
    ///
    /// The backend must implement the storage contract's command ledger (`command_get`,
    /// `command_put`, `command_prune`; storage contract 0.3.0): one that does not makes every
    /// write with a command id fail with the mapped `Unsupported` error rather than run without
    /// idempotency.
    #[must_use]
    pub fn with_command_ledger(mut self, policy: RetentionPolicy) -> Self {
        self.ledger = Some(policy);
        self
    }

    /// The ledger's retention policy, if the engine keeps a ledger.
    #[must_use]
    pub fn command_retention(&self) -> Option<&RetentionPolicy> {
        self.ledger.as_ref()
    }

    /// A handle that runs every write under the retention class `class` (`bpm`, `import`,
    /// `http_api`, `jobs`, `scripts` or a class the site defines). The host sets it once per entry
    /// point. A class the policy does not know is the default class.
    ///
    /// The handle implements [`DocumentService`]; reads and field changes are the engine's own.
    /// It does not return receipts (the [`DocumentService`] methods never did).
    #[must_use]
    pub fn for_class(&self, class: &str) -> Classed<'_, B> {
        Classed {
            engine: self,
            class: class.to_owned(),
        }
    }

    /// Deletes every ledger row whose `retain_until` is at or before `now` and returns how many.
    /// It looks at `retain_until` only, never at `recorded_at`, so a row written under a longer
    /// policy than today's is kept until its own date. The jobs service calls it (hosting it is
    /// WP-PLT-05.T9); `now` is an argument because the library never reads the wall clock.
    ///
    /// # Errors
    ///
    /// The mapped storage error, `Unsupported` among them.
    pub async fn prune_commands(&self, now: OffsetDateTime) -> AppResult<u64> {
        let snap = self.snapshot();
        let map = |e: spot_storage_contract::StorageError| map_storage_error(&e, StorageOp::Other);
        let options = TxOptions::new(label("command_ledger", "prune"), Isolation::ReadCommitted);
        let mut tx = self
            .backend
            .begin(snap.catalog.clone(), options)
            .await
            .map_err(map)?;
        let pruned = match tx.command_prune(now).await {
            Ok(n) => n,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(map(e));
            }
        };
        tx.commit().await.map_err(map)?;
        Ok(pruned)
    }

    /// What the ledger needs of this request, if it is covered.
    pub(crate) fn ledger_call(
        &self,
        inv: &Invocation,
        req: &Req,
        class: Option<&str>,
    ) -> Option<LedgerCall> {
        self.ledger.as_ref()?;
        let command_id = inv.command_id.as_ref()?.as_str().to_owned();
        Some(LedgerCall {
            principal: inv.actor.principal_text(),
            command_id,
            fingerprint: fingerprint(req)?,
            class: class.map(str::to_owned),
        })
    }

    /// The record to write with the operation: its outcome (identity and messages) and the
    /// retention of the caller's class, computed now.
    pub(crate) fn command_record(
        &self,
        call: &LedgerCall,
        req: &Req,
        value: &Ret,
        messages: &[Message],
    ) -> AppResult<CommandRecord> {
        let policy = self.ledger.as_ref().ok_or_else(|| invalid("no policy"))?;
        let (doctype, id, version) = match value {
            Ret::Doc(doc) | Ret::DocJson(doc, _) => {
                (doc.doctype().as_str().to_owned(), doc.id(), doc.version())
            }
            Ret::Unit => (
                req.doctype().to_owned(),
                req.target_id().ok_or_else(|| invalid("no target"))?,
                None,
            ),
        };
        let outcome = json!({
            "v": OUTCOME_VERSION,
            "operation": req.op(),
            "doctype": doctype,
            "id": hex(id.as_bytes()),
            "version": version.map(|v| v.0),
            "messages": messages.iter().map(message_json).collect::<Vec<_>>(),
        })
        .to_string();
        let recorded_at = self.clock.now_utc();
        let retain_until = policy.retain_until(call.class.as_deref(), recorded_at);
        CommandRecord::new(
            call.principal.clone(),
            call.command_id.clone(),
            call.fingerprint.clone(),
            outcome,
            recorded_at,
            retain_until,
        )
        .map_err(|e| map_storage_error(&e, StorageOp::Write))
    }

    /// After a failed operation: whether a concurrent first use of the same command id has
    /// committed meanwhile. `Ok(Some(record))` is a replay, `Ok(None)` means the failure stands,
    /// and a record of another request is `document.command_conflict`.
    pub(crate) async fn recorded_since(
        &self,
        catalog: &Arc<spot_storage_contract::Catalog>,
        op_label: &spot_app_api::prelude::Name,
        call: &LedgerCall,
    ) -> AppResult<Option<CommandRecord>> {
        let options = TxOptions::new(op_label.clone(), Isolation::ReadCommitted).read_only();
        let Ok(tx) = self.backend.begin(catalog.clone(), options).await else {
            return Ok(None);
        };
        let found = tx.command_get(&call.principal, &call.command_id).await;
        let _ = tx.rollback().await;
        match found {
            Ok(Some(record)) if record.fingerprint == call.fingerprint => Ok(Some(record)),
            Ok(Some(_)) => Err(command_conflict()),
            Ok(None) | Err(_) => Ok(None),
        }
    }

    /// Answers a repeated command from its stored outcome: the identity and messages as first
    /// answered, with the document loaded now under the caller's own read permission (field
    /// masks apply). It runs no participant, step, hook, write, outbox event or audit row.
    pub(crate) async fn replayed(
        &self,
        inv: &Invocation,
        req: &Req,
        record: &CommandRecord,
    ) -> AppResult<Done> {
        let stored = Stored::parse(&record.outcome)?;
        let snap = self.snapshot();
        let value = if stored.operation == "delete" {
            Ret::Unit
        } else {
            let doc = self
                .read_document(&snap, &stored.doctype, stored.id)
                .await?
                .ok_or_else(not_found)?;
            let principal = self.authorize(inv, Action::Read, &doc).await?;
            Ret::Doc(self.readable(&principal, doc).await?)
        };
        let _ = req;
        let receipt = ReceiptRecord::empty(
            operation_of(&stored.operation),
            &stored.doctype,
            stored.id.to_string(),
            stored.version.unwrap_or(Version(0)),
            record.recorded_at,
        );
        Ok(Done {
            value,
            messages: stored.messages,
            receipts: vec![Finished {
                record: receipt,
                depth: 0,
            }],
        })
    }
}

/// The engine seen through one retention class: every write is recorded in the command ledger
/// under that class's retention. See [`Engine::for_class`].
pub struct Classed<'e, B: StorageBackend> {
    engine: &'e Engine<B>,
    class: String,
}

impl<B: StorageBackend> Classed<'_, B> {
    /// The retention class this handle writes under.
    #[must_use]
    pub fn class(&self) -> &str {
        &self.class
    }
}

impl<B: StorageBackend> DocumentService for Classed<'_, B> {
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        DocumentService::meta(self.engine, doctype)
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        DocumentService::load(self.engine, inv, doctype, id)
    }

    fn create<'a>(
        &'a self,
        inv: &'a Invocation,
        doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let done = self
                .engine
                .top_class(inv, Req::Create(doc), Some(&self.class))
                .await?;
            done.into_doc()
        })
    }

    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        let req = Req::Update {
            doctype: doctype.to_owned(),
            id,
            patch,
            expected,
        };
        Box::pin(async move {
            self.engine
                .top_class(inv, req, Some(&self.class))
                .await?
                .into_doc()
        })
    }

    fn post<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        let req = Req::Post {
            doctype: doctype.to_owned(),
            id,
            expected,
        };
        Box::pin(async move {
            self.engine
                .top_class(inv, req, Some(&self.class))
                .await?
                .into_doc()
        })
    }

    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        let req = Req::Cancel {
            doctype: doctype.to_owned(),
            id,
            expected,
        };
        Box::pin(async move {
            self.engine
                .top_class(inv, req, Some(&self.class))
                .await?
                .into_doc()
        })
    }

    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        let req = Req::Correct {
            doctype: doctype.to_owned(),
            id,
            expected,
        };
        Box::pin(async move {
            self.engine
                .top_class(inv, req, Some(&self.class))
                .await?
                .into_doc()
        })
    }

    fn delete<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        let req = Req::Delete {
            doctype: doctype.to_owned(),
            id,
            expected,
        };
        Box::pin(async move {
            self.engine
                .top_class(inv, req, Some(&self.class))
                .await?
                .into_unit()
        })
    }

    fn rename<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        let req = Req::Rename {
            doctype: doctype.to_owned(),
            id,
            new_name: new_name.to_owned(),
            expected,
        };
        Box::pin(async move {
            self.engine
                .top_class(inv, req, Some(&self.class))
                .await?
                .into_doc()
        })
    }

    fn call_method<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: serde_json::Value,
        expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, serde_json::Value)>>> {
        DocumentService::call_method(self.engine, inv, doctype, id, method, args, expected)
    }

    fn field_changed<'a>(
        &'a self,
        inv: &'a Invocation,
        draft: Document,
        field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        DocumentService::field_changed(self.engine, inv, draft, field)
    }
}
