//! The audit event: what changed, who changed it and when, as the outbox carries it.
//!
//! A [`DocAuditEvent`] is built from the before and after images of one document write. The diff
//! holds changed fields only; a secured field's old and new value are already in their masked
//! form (ADR-070 section 12 rule 9), and a long text or attachment is recorded as a length and a
//! digest rather than its content. An event is capped: a larger diff is replaced by the digest of
//! the full diff plus the first [`EventSizePolicy::max_changes`] changes and `truncated = true`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spot_activity::FieldChange;
use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{Name, Value};
use spot_app_api::{Actor, Document};

use crate::error::DocAuditError;
use crate::masker::FieldMasker;

/// The default cap on an encoded event, in bytes (32 KiB).
pub const MAX_EVENT_BYTES: usize = 32 * 1024;
/// The default number of changes kept when an event is truncated.
pub const MAX_CHANGES: usize = 100;
/// The default size above which a text value is recorded as `{len, digest}`.
pub const LONG_VALUE_BYTES: usize = 256;

/// A lifecycle operation an audit event records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocOperation {
    /// Create.
    Create,
    /// Update of a draft.
    Update,
    /// Delete.
    Delete,
    /// Post.
    Post,
    /// Cancel.
    Cancel,
    /// Correct (cancel and amend, ADR-42).
    Correct,
    /// Rename.
    Rename,
    /// A whitelisted method that changed the document; the method name is in the payload.
    Method(String),
}

impl DocOperation {
    /// The chain kind segment: `create`, `update`, ..., or `method`.
    #[must_use]
    pub fn kind_segment(&self) -> &str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Post => "post",
            Self::Cancel => "cancel",
            Self::Correct => "correct",
            Self::Rename => "rename",
            Self::Method(_) => "method",
        }
    }

    /// The text form, for the payload.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Method(name) if !name.is_empty() => name,
            other => other.kind_segment(),
        }
    }
}

/// Where a change came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditSource {
    /// The native API.
    Api,
    /// A job.
    Job,
    /// An import.
    Import,
    /// Sync apply.
    Sync,
    /// A script.
    Script,
}

impl AuditSource {
    /// The text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Job => "job",
            Self::Import => "import",
            Self::Sync => "sync",
            Self::Script => "script",
        }
    }
}

/// The acting identity, in the form the chain's [`AuditRecord`](spot_audit_chain::AuditRecord)
/// accepts, plus the platform component for a system actor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    /// `system`, `user:<id>`, `consumer:<id>` or `agent:<id>`.
    pub actor: String,
    /// With impersonation, the identity acted on behalf of.
    pub on_behalf_of: Option<String>,
    /// For a system actor, the component (`scheduler`, `sync_apply`, ...).
    pub component: Option<String>,
}

impl ActorRef {
    /// The acting identity of an invocation.
    #[must_use]
    pub fn of(actor: &Actor) -> Self {
        match actor {
            Actor::User { id, via } => Self {
                actor: format!("user:{id}"),
                on_behalf_of: via.map(|v| format!("user:{v}")),
                component: None,
            },
            Actor::Consumer { id } => Self {
                actor: format!("consumer:{id}"),
                on_behalf_of: None,
                component: None,
            },
            Actor::System { component } => Self {
                actor: "system".to_owned(),
                on_behalf_of: None,
                component: Some(component.as_str().to_owned()),
            },
        }
    }
}

/// One value of a diff side. A large text or an attachment is a length and a digest, not content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffValue {
    /// No value.
    Null,
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// Text within the size bound.
    Text(String),
    /// A value recorded by length and digest: `{len, digest}`.
    Digest {
        /// The byte length of the masked-or-plain stored form.
        len: u64,
        /// The SHA-256 of that form, as lower-case hex.
        digest: String,
    },
}

/// One changed field, with its masked old and new value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffChange {
    /// The path: `field`, `table[id=<id>].col`, or `table[id=<id>]` for a whole row.
    pub path: String,
    /// The value before, masked; `None` when absent or null.
    pub before: Option<DiffValue>,
    /// The value after, masked; `None` when absent or null.
    pub after: Option<DiffValue>,
}

/// What one document write looks like to the audit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocAuditEvent {
    /// The site.
    pub site: String,
    /// The DocType.
    pub doctype: String,
    /// The document id.
    pub id: String,
    /// The version the write produced.
    pub version: u64,
    /// The operation.
    pub operation: DocOperation,
    /// The acting identity.
    pub actor: ActorRef,
    /// Unix microseconds.
    pub at: i64,
    /// The names of the fields and tables that changed.
    pub changed_fields: Vec<String>,
    /// The masked old and new value of every changed field.
    pub diff: Vec<DiffChange>,
    /// Some changes were dropped because the reader may not see the field.
    pub hidden_changes: bool,
    /// The command id (the API's `Idempotency-Key`), when there was one.
    pub command_id: Option<String>,
    /// Where the change came from.
    pub source: AuditSource,
    /// The diff was too large and is truncated.
    pub truncated: bool,
    /// When truncated, the SHA-256 of the full diff, as lower-case hex.
    pub diff_digest: Option<String>,
    /// For a whitelisted method's write, the declared method name (1 to 64 bytes of
    /// `[a-z0-9_]`; a name that is not is dropped, never recorded). Schema 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// For a [`DocOperation::Correct`], which half this is: `cancel` or `amend`. Schema 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction_half: Option<String>,
    /// How the write came about when the operation alone does not say: `step` for a write a
    /// lifecycle step made, `method` for a method write whose name was dropped. Schema 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// The deployment node (ADR-062) that made the write, when the site has more than one.
    /// Schema 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// `denied` for a refused operation; absent for a write that was made. A denial holds no
    /// diff, no field value and no message. Schema 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// For a denial, the class of the refusal (its native API code). Schema 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    /// For a denial, the stable code of the refusal. Schema 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// The `outcome` of a refused operation.
pub const OUTCOME_DENIED: &str = "denied";

/// The longest method name an event records, in bytes.
pub const MAX_METHOD_BYTES: usize = 64;

/// Whether `name` is a method name the audit will record: 1 to [`MAX_METHOD_BYTES`] bytes of
/// `[a-z0-9_]`. Anything else is dropped, because the audit does not trust a name it cannot
/// bound.
#[must_use]
pub fn is_recordable_method(name: &str) -> bool {
    (1..=MAX_METHOD_BYTES).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The schema 2 additions of an event, set before the size cap is applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventExtra {
    /// See [`DocAuditEvent::method`].
    pub method: Option<String>,
    /// See [`DocAuditEvent::correction_half`].
    pub correction_half: Option<String>,
    /// See [`DocAuditEvent::via`].
    pub via: Option<String>,
    /// See [`DocAuditEvent::node`].
    pub node: Option<String>,
    /// See [`DocAuditEvent::outcome`].
    pub outcome: Option<String>,
    /// See [`DocAuditEvent::error_class`].
    pub error_class: Option<String>,
    /// See [`DocAuditEvent::error_code`].
    pub error_code: Option<String>,
}

/// The size rules an event is built and capped under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventSizePolicy {
    /// The cap on the encoded event, in bytes.
    pub max_bytes: usize,
    /// How many changes survive truncation.
    pub max_changes: usize,
    /// The size above which a text value becomes `{len, digest}`.
    pub long_value_bytes: usize,
}

impl Default for EventSizePolicy {
    fn default() -> Self {
        Self {
            max_bytes: MAX_EVENT_BYTES,
            max_changes: MAX_CHANGES,
            long_value_bytes: LONG_VALUE_BYTES,
        }
    }
}

/// A document image: what a diff compares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentImage {
    /// The DocType.
    pub doctype: String,
    /// The document id.
    pub id: String,
    /// The version.
    pub version: u64,
    /// The parent fields.
    pub fields: BTreeMap<Name, Value>,
    /// The child tables.
    pub tables: BTreeMap<Name, Vec<ChildRow>>,
}

impl DocumentImage {
    /// The image of a loaded document.
    #[must_use]
    pub fn of(doc: &Document) -> Self {
        Self {
            doctype: doc.doctype().as_str().to_owned(),
            id: doc.id().to_string(),
            version: doc.version().map_or(0, |v| v.0),
            fields: doc.fields().clone(),
            tables: doc.tables().clone(),
        }
    }

    /// An empty image (every field of the after image then counts as set).
    #[must_use]
    pub fn empty(doctype: &str, id: &str) -> Self {
        Self {
            doctype: doctype.to_owned(),
            id: id.to_owned(),
            version: 0,
            fields: BTreeMap::new(),
            tables: BTreeMap::new(),
        }
    }

    /// An image with no parent fields and no tables.
    #[must_use]
    pub fn new(doctype: &str, id: &str, version: u64) -> Self {
        Self {
            doctype: doctype.to_owned(),
            id: id.to_owned(),
            version,
            fields: BTreeMap::new(),
            tables: BTreeMap::new(),
        }
    }
}

/// Builds the audit event for one write: the diff of `before` and `after`, masked, capped and
/// ready for the outbox.
///
/// # Errors
/// [`DocAuditError::UnknownDoctype`] or [`DocAuditError::Guard`] from the masker,
/// [`DocAuditError::Encoding`] when the event cannot be encoded.
#[allow(clippy::too_many_arguments)]
pub fn build_event(
    masker: &dyn FieldMasker,
    site: &str,
    operation: DocOperation,
    after: &DocumentImage,
    before: Option<&DocumentImage>,
    actor: ActorRef,
    at: i64,
    command_id: Option<String>,
    source: AuditSource,
    policy: &EventSizePolicy,
) -> Result<DocAuditEvent, DocAuditError> {
    build_event_with(
        masker,
        site,
        operation,
        after,
        before,
        actor,
        at,
        command_id,
        source,
        policy,
        EventExtra::default(),
    )
}

/// [`build_event`] with the schema 2 additions in `extra`.
///
/// # Errors
/// As [`build_event`].
#[allow(clippy::too_many_arguments)]
pub fn build_event_with(
    masker: &dyn FieldMasker,
    site: &str,
    operation: DocOperation,
    after: &DocumentImage,
    before: Option<&DocumentImage>,
    actor: ActorRef,
    at: i64,
    command_id: Option<String>,
    source: AuditSource,
    policy: &EventSizePolicy,
    extra: EventExtra,
) -> Result<DocAuditEvent, DocAuditError> {
    let changes = diff_images(before, after);
    let masked = masker.mask_for_audit(&after.doctype, &changes)?;
    let changed_fields: Vec<String> = {
        let set: BTreeSet<String> = masked
            .changes
            .iter()
            .map(|c| changed_name(&c.path))
            .collect();
        set.into_iter().collect()
    };
    let diff: Vec<DiffChange> = masked
        .changes
        .iter()
        .map(|c| diff_change(c, policy.long_value_bytes))
        .collect();
    let mut event = DocAuditEvent {
        site: site.to_owned(),
        doctype: after.doctype.clone(),
        id: after.id.clone(),
        version: after.version,
        operation,
        actor,
        at,
        changed_fields,
        diff,
        hidden_changes: masked.hidden_changes,
        command_id,
        source,
        truncated: false,
        diff_digest: None,
        method: extra.method,
        correction_half: extra.correction_half,
        via: extra.via,
        node: extra.node,
        outcome: extra.outcome,
        error_class: extra.error_class,
        error_code: extra.error_code,
    };
    cap_event(&mut event, policy)?;
    Ok(event)
}

/// Truncates an event that is too large: the digest of the full diff plus the first
/// `policy.max_changes` changes, with `truncated = true`.
///
/// # Errors
/// [`DocAuditError::Encoding`] when the full diff cannot be encoded.
pub fn cap_event(event: &mut DocAuditEvent, policy: &EventSizePolicy) -> Result<(), DocAuditError> {
    let encoded = encode_event(event)?;
    if encoded.len() <= policy.max_bytes {
        return Ok(());
    }
    let full =
        serde_json::to_string(&event.diff).map_err(|e| DocAuditError::Encoding(e.to_string()))?;
    event.diff_digest = Some(sha256_hex(full.as_bytes()));
    event.diff.truncate(policy.max_changes);
    event.truncated = true;
    Ok(())
}

/// The canonical JSON of an event.
///
/// # Errors
/// [`DocAuditError::Encoding`] when serde fails.
pub fn encode_event(event: &DocAuditEvent) -> Result<String, DocAuditError> {
    serde_json::to_string(event).map_err(|e| DocAuditError::Encoding(e.to_string()))
}

/// Decodes an event from its JSON.
///
/// # Errors
/// [`DocAuditError::Encoding`] when the JSON is not an event.
pub fn decode_event(json: &str) -> Result<DocAuditEvent, DocAuditError> {
    serde_json::from_str(json).map_err(|e| DocAuditError::Encoding(e.to_string()))
}

/// The SHA-256 of `bytes`, as lower-case hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// The field or table name a diff path points at: `items[id=..].qty` -> `items`.
fn changed_name(path: &str) -> String {
    match path.split_once('[') {
        Some((table, _)) => table.to_owned(),
        None => path.to_owned(),
    }
}

pub(crate) fn diff_change(change: &FieldChange, long: usize) -> DiffChange {
    DiffChange {
        path: change.path.clone(),
        before: change.before.as_ref().map(|v| diff_value(v, long)),
        after: change.after.as_ref().map(|v| diff_value(v, long)),
    }
}

/// A value as a diff value: a large text or a byte string becomes a length and a digest.
fn diff_value(value: &Value, long: usize) -> DiffValue {
    match value {
        Value::Null => DiffValue::Null,
        Value::Bool(b) => DiffValue::Bool(*b),
        Value::Int(i) => DiffValue::Int(*i),
        Value::Text(t) => {
            if t.len() > long {
                digest_value(t.as_bytes())
            } else {
                DiffValue::Text(t.clone())
            }
        }
        Value::Decimal(d) => DiffValue::Text(d.to_string()),
        Value::Id(id) => DiffValue::Text(id.to_string()),
        Value::Date(d) => DiffValue::Text(d.to_string()),
        Value::Time(t) => DiffValue::Text(t.to_string()),
        Value::DateTime(t) => DiffValue::Text(t.to_string()),
        Value::Json(j) => {
            if j.len() > long {
                digest_value(j.as_bytes())
            } else {
                DiffValue::Text(j.clone())
            }
        }
        Value::Bytes(b) => digest_value(b),
        // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug text.
        Value::GeoPoint(_) | Value::Range(_) => DiffValue::Text(format!("{value:?}")),
    }
}

fn digest_value(bytes: &[u8]) -> DiffValue {
    DiffValue::Digest {
        len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        digest: sha256_hex(bytes),
    }
}

/// The changed fields and rows of two images, as the field guard reads them.
///
/// A field that is absent on one side is `None`; a row added or removed is one change whose path
/// is the row and whose present side is a JSON row snapshot; a shared row diffs column by column.
/// The result is sorted by path for determinism.
#[must_use]
pub fn diff_images(before: Option<&DocumentImage>, after: &DocumentImage) -> Vec<FieldChange> {
    let empty = DocumentImage::default();
    let before = before.unwrap_or(&empty);
    let mut changes: Vec<FieldChange> = Vec::new();

    let mut names: BTreeSet<&str> = before.fields.keys().map(Name::as_str).collect();
    names.extend(after.fields.keys().map(Name::as_str));
    for name in names {
        let b = field_value(&before.fields, name);
        let a = field_value(&after.fields, name);
        if b != a {
            changes.push(FieldChange {
                path: name.to_owned(),
                before: side(b),
                after: side(a),
            });
        }
    }

    let mut tables: BTreeSet<&str> = before.tables.keys().map(Name::as_str).collect();
    tables.extend(after.tables.keys().map(Name::as_str));
    for table in tables {
        let br = table_rows(&before.tables, table);
        let ar = table_rows(&after.tables, table);
        let by_id: BTreeMap<String, &ChildRow> = br.iter().map(|r| (r.id.to_string(), r)).collect();
        let ay_id: BTreeMap<String, &ChildRow> = ar.iter().map(|r| (r.id.to_string(), r)).collect();
        for (id, row) in &by_id {
            if !ay_id.contains_key(id) {
                changes.push(row_change(table, id, Some(*row), None));
            }
        }
        for (id, row) in &ay_id {
            match by_id.get(id) {
                None => changes.push(row_change(table, id, None, Some(*row))),
                Some(old) => {
                    let mut cols: BTreeSet<&str> = old.fields.keys().map(Name::as_str).collect();
                    cols.extend(row.fields.keys().map(Name::as_str));
                    for col in cols {
                        let b = field_value(&old.fields, col);
                        let a = field_value(&row.fields, col);
                        if b != a {
                            changes.push(FieldChange {
                                path: format!("{table}[id={id}].{col}"),
                                before: side(b),
                                after: side(a),
                            });
                        }
                    }
                }
            }
        }
    }

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

fn field_value<'m>(map: &'m BTreeMap<Name, Value>, field: &str) -> &'m Value {
    map.iter()
        .find(|(k, _)| k.as_str() == field)
        .map_or(&Value::Null, |(_, v)| v)
}

fn table_rows<'m>(tables: &'m BTreeMap<Name, Vec<ChildRow>>, table: &str) -> &'m [ChildRow] {
    tables
        .iter()
        .find(|(k, _)| k.as_str() == table)
        .map_or(&[], |(_, v)| v.as_slice())
}

fn side(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        other => Some(other.clone()),
    }
}

fn row_change(
    table: &str,
    id: &str,
    before: Option<&ChildRow>,
    after: Option<&ChildRow>,
) -> FieldChange {
    FieldChange {
        path: format!("{table}[id={id}]"),
        before: before.map(row_snapshot),
        after: after.map(row_snapshot),
    }
}

/// A whole-row snapshot as JSON: `{"id": .., "fields": {..}}`.
fn row_snapshot(row: &ChildRow) -> Value {
    let mut fields = serde_json::Map::new();
    for (k, v) in &row.fields {
        fields.insert(k.as_str().to_owned(), value_to_json(v));
    }
    let json = serde_json::json!({ "id": row.id.to_string(), "fields": fields });
    Value::Json(json.to_string())
}

/// A stored value as JSON, for a row snapshot.
#[must_use]
pub fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Decimal(d) => serde_json::Value::String(d.to_string()),
        Value::Text(t) => serde_json::Value::String(t.clone()),
        Value::Date(d) => serde_json::Value::String(d.to_string()),
        Value::Time(t) => serde_json::Value::String(t.to_string()),
        Value::DateTime(t) => serde_json::Value::String(t.to_string()),
        Value::Id(id) => serde_json::Value::String(id.to_string()),
        Value::Json(j) => {
            serde_json::from_str(j).unwrap_or_else(|_| serde_json::Value::String(j.clone()))
        }
        Value::Bytes(b) => serde_json::Value::String(sha256_hex(b)),
        // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug text.
        Value::GeoPoint(_) | Value::Range(_) => serde_json::Value::String(format!("{value:?}")),
    }
}
