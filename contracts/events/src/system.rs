//! The platform's `sys_*` topics that carry a payload schema (ADR-056, amendments A3 and A4).
//!
//! Three platform crates emit or consume an event shape that this contract now registers:
//!
//! | Topic | Producer | Consumer | Delivery | Retention |
//! | --- | --- | --- | --- | --- |
//! | [`DOC_AUDIT_TOPIC`] | `spot-doc-audit` | the audit chain and the activity views | at-least-once | audit |
//! | [`GRAPH_EDGE_TOPIC`] | `spot-edge-sync` | the graph projector (WP-GRA-07) | at-least-once | outbox |
//! | [`SECRET_AUDIT_TOPIC`] | `spot-document` (the secret vault) | the audit chain and the access reviews | at-least-once | audit |
//!
//! All three are **v1** and all three are written in the same transaction as the change they
//! describe (ADR-11). A consumer deduplicates on the event id. This module defines the shape
//! only; emission lives in `spot-doc-audit`, `spot-edge-sync` and `spot-document`, decoding in
//! the projector and in the audit readers.
//!
//! # The document audit event (`sys_doc_audit`)
//!
//! The payload is the serialised `DocAuditEvent` of `spot-doc-audit` (the contract adapts to the
//! crate, not the other way round). It names the document (`doctype`, `id`), the `version` the
//! write produced, the `operation`, the `actor`, the `source`, and the **masked** diff: only
//! changed fields, each with its before and after value. A secured field's values are already
//! masked by the producer (ADR-070 section 12 rule 9); the contract cannot see that, so it
//! bounds what a diff may hold:
//!
//! - a diff value is one of `"null"`, `{"bool": b}`, `{"int": n}`, `{"text": s}` (at most
//!   [`MAX_DIFF_TEXT_CHARS`]) or `{"digest": {"len": n, "digest": <64 hex>}}` (a long text or an
//!   attachment is recorded by length and digest, never by content);
//! - at most [`MAX_DIFF_CHANGES`] diff entries and [`MAX_CHANGED_FIELDS`] changed field names;
//! - when the producer compacts a diff it sets `truncated` and `diff_digest` (the SHA-256 of the
//!   full diff, lower-case hex) together: one without the other is invalid;
//! - the only free text anywhere is the bounded `text` value of a diff entry.
//!
//! ## The schema 2 additions (ADR-056 amendment A6)
//!
//! Amendment A6 adds the seven optional fields of the audit participant's schema 2
//! (`spot-doc-audit`, WP-SEC-06.T3) to the same v1 payload. Every one is optional, so a schema 1
//! payload validates unchanged, and every one is bounded, because a denial is written by anyone
//! who can make a refused request:
//!
//! - `method` is the declared method of a method-made write: 1 to [`MAX_METHOD_BYTES`] bytes of
//!   `[a-z0-9_]`, and the `operation` names the same method (`{"method": name}`). The producer
//!   drops a name it cannot bound, so a name that arrives here is always of that shape.
//! - `correction_half` is `cancel` or `amend` (ADR-42) and only accompanies operation `correct`.
//! - `via` is how the write came about when the operation alone does not say: `step` for a
//!   lifecycle step, `method` for a method whose name was dropped. A closed set, because a
//!   consumer switches on it and an open value is a way to carry an arbitrary string.
//! - `node` is the deployment node (ADR-062) that made the write, as a name.
//! - `outcome` has exactly one value, `denied`, and a denial is the access attempt the audit
//!   exists for: **no diff, no changed field, no digest, no truncation and no message**. There is
//!   no field for a message at all, and an unknown field is refused like any other.
//! - `error_class` and `error_code` are the refusal's class and stable code, as bounded catalogue
//!   codes (`namespace.reason`, never a message), and they are allowed only with `outcome =
//!   "denied"`.
//!
//! The previous version is not a field: an update that produced version `n` replaced version
//! `n - 1`, and the previous image is in the audit chain. The subject version of the outbox
//! record equals `version`.
//!
//! # The graph edge event (`sys_graph_edge`)
//!
//! One event is one change to one edge: `op` is `add`, `remove` or `update`, `edge_type` is the
//! canonical edge-type name of the graph model, and `from` and `to` are the two ends in
//! **storage orientation** (`from` is the record holding the link, `to` the record it points at).
//! `owner` is present only for a relation edge, owned by its relation record. `properties`
//! holds the edge's properties **after** the event, each a tagged typed value (`{"decimal":
//! "10.50"}`, `{"int": 3}`, ...), never a float. `holder_version` is the version of the holder
//! (`from`) document that produced the change; for a delete it is the last version before it.
//! `remove` carries no properties.
//!
//! ## Where the sequence comes from
//!
//! The payload has **no sequence field**. The sequence of an `EdgeEvent` is the position of the
//! outbox record that carries the event (the feed position's sequence component), which the
//! storage contract already orders and makes durable (ADR-053). The projector derives
//! `EdgeEvent::sequence` from the record position and the graph's tenant from the record's
//! source. A position is comparable only within one source and one stream generation; a new
//! generation or a hole means the projection rebuilds. See the amendment for the open question
//! about holes when the outbox is shared with other topics.
//!
//! # The secret field audit event (`sys_secret_audit`)
//!
//! One event is one secret-field call of `spot-document` (`set_secret`, `create_with_secrets`,
//! `open_secret`, `find_by_secret`, `rewrap_secrets`, `secret_is_set`), written in the
//! transaction of the call (ADR-11) or, for a call that changed nothing, in a transaction of its
//! own. The payload names the call; it never carries the secret:
//!
//! - `op` is the call: `set`, `open`, `find`, `rewrap` or `is_set` (closed; `create_with_secrets`
//!   writes one `set` event per field);
//! - `doctype` and `field` are **names**: `field` is a secret field of `doctype`, or `"*"` for one
//!   row of a rewrap batch (every secret field of that record);
//! - `record` is the record the call named, absent when the call named none (the subject of the
//!   outbox event is always a node, so a call without a record still has a subject id);
//! - `caller` is the capability that made the call: `login_service`, `seed`, `webhooks`,
//!   `operator` or `test` (`test` exists only in a build with the `testing` feature);
//! - `site` is the site of the invocation; the actor is the event's own `meta.actor`, never a
//!   payload field;
//! - `outcome` is `ok` or `refused`; a refusal carries `error`, the catalogue code the call
//!   ended with (`secretfield.open_failed`, `document.version_conflict`, ...), and a success
//!   carries none;
//! - `state` is what the engine knows of the secret afterwards: `<set>`, `<unset>` or `<none>`
//!   (a call that failed changed nothing it can vouch for, so it records `<none>`).
//!
//! There is no field for a value, a sealed blob (`__sealed`), a blind index (`__bidx`), an
//! initialisation vector, a key or a key id, and [`validate`] refuses a payload that looks like
//! one: `doctype` and `field` must be names (a field name may be `"*"`), no name may end in a
//! derived-column suffix or hold a hex run of 16 characters or more, and `error` must be a
//! catalogue code. A sealed value is carried by its record, never by its audit event.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use crate::identity::FORBIDDEN_KEY_PARTS;
use crate::names::Topic;
use crate::payload::{PrincipalKind, PrincipalRef, SYSTEM_PRINCIPAL};
use crate::schema::{EventSchema, FieldSchema, PayloadType, SchemaError, is_date, is_utc_datetime};
use crate::system_platform as platform;

/// The topic of the document audit event.
pub const DOC_AUDIT_TOPIC: &str = "sys_doc_audit";
/// The topic of the graph edge event.
pub const GRAPH_EDGE_TOPIC: &str = "sys_graph_edge";
/// The topic of the secret field audit event.
pub const SECRET_AUDIT_TOPIC: &str = "sys_secret_audit";
/// Every `sys_*` topic with a registered payload schema: the three above, then the nine of
/// [`crate::system_platform`] (0.7.2), then the five of [`crate::journal`] (0.7.6).
pub const TOPICS: [&str; 17] = [
    DOC_AUDIT_TOPIC,
    GRAPH_EDGE_TOPIC,
    SECRET_AUDIT_TOPIC,
    platform::JOB_ENQUEUED_TOPIC,
    platform::USER_NOTICE_TOPIC,
    platform::OPERATION_RECEIPT_TOPIC,
    platform::OPERATION_RECEIPT_EFFECT_TOPIC,
    platform::PARTITION_RANGE_LOW_TOPIC,
    platform::BPM_GOVERNANCE_TOPIC,
    platform::BPM_CASE_MIGRATED_TOPIC,
    platform::CONDITION_RAISED_TOPIC,
    platform::CONDITION_CLEARED_TOPIC,
    crate::journal::JOURNAL_ACCESS_TOPIC,
    crate::journal::JOURNAL_GRANT_TOPIC,
    crate::journal::JOURNAL_ADMIN_TOPIC,
    crate::journal::SYNC_NODE_ADMIN_TOPIC,
    crate::journal::SYNC_DECISION_TOPIC,
];

/// The revision of the document audit event's v1 payload, in the schema registry only. The
/// seven optional schema 2 fields raised it from 0 (ADR-056 amendment A6).
pub const DOC_AUDIT_REVISION: u32 = 1;

/// The revision of the secret audit event's v1 payload, in the schema registry only. Its closed
/// `caller` set gained `webhooks` and `operator` at revision 1 (ADR-056 amendment A7).
pub const SECRET_AUDIT_REVISION: u32 = 1;

/// The longest string a `sys_*` payload may hold, in characters, at any depth, except a diff
/// `text` value (see [`MAX_DIFF_TEXT_CHARS`]).
pub const MAX_STRING_CHARS: usize = 256;
/// The longest `text` value of an audit diff. The producer records anything longer as a length
/// and digest (its default threshold is 256 bytes).
pub const MAX_DIFF_TEXT_CHARS: usize = 1024;
/// The most entries an audit diff may hold. The producer truncates to 100 by default.
pub const MAX_DIFF_CHANGES: usize = 1000;
/// The most changed field names an audit event may list.
pub const MAX_CHANGED_FIELDS: usize = 1024;
/// The most properties an edge event may carry.
pub const MAX_EDGE_PROPERTIES: usize = 64;
/// The longest DocType name or secret field name a secret audit event may carry. The names of
/// this platform are at most 63 characters (see [`Topic`]); a longer string is not a name.
pub const MAX_NAME_CHARS: usize = 63;
/// The longest method name an audit event records, in bytes (the producer's
/// `MAX_METHOD_BYTES`).
pub const MAX_METHOD_BYTES: usize = 64;

/// Delivery of every `sys_*` event registered here (ADR-056, amendments A3 and A4):
/// `at-least-once`.
pub const AT_LEAST_ONCE: bool = true;

/// How long a topic's events are kept (the registry's type, re-exported for 0.6.0 callers).
pub use crate::registry::Retention;

/// The retention of a `sys_*` topic registered here, or `None` for another topic.
#[deprecated(
    since = "0.7.1",
    note = "use `registry::retention_of`; removed in 0.8.0"
)]
pub fn retention_of(topic: &str) -> Option<Retention> {
    if TOPICS.contains(&topic) {
        crate::registry::retention_of(topic)
    } else {
        None
    }
}

/// Whether the event is an audit record (kept per the audit retention).
#[deprecated(since = "0.7.1", note = "use `registry::is_audit`; removed in 0.8.0")]
pub fn is_audit(topic: &str) -> bool {
    TOPICS.contains(&topic) && crate::registry::is_audit(topic)
}

/// `Text` and `Json` fields that are free-form by design (the same rule as
/// [`identity::FREE_FORM_FIELDS`](crate::identity::FREE_FORM_FIELDS)). A contract test fails on
/// any other `Text` or `Json` field in a `sys_*` schema.
///
/// Every entry is a name or an identifier, a digest, or a bounded value checked by
/// [`validate`]; none is prose. `operation`, `before`, `after` and `properties` are `Json` fields
/// whose shape [`validate`] checks structurally; `field`, `error`, `doctype` and `site` are
/// `Text` and every one of them is checked too: the secret audit event's `doctype` and `field`
/// must be names, and its `error` must be a catalogue code. The audit event's `method` is a
/// bounded method name, `node` a deployment node name, and `error_class` and `error_code` are
/// bounded catalogue codes.
pub const FREE_FORM_FIELDS: &[&str] = &[
    "site",
    "doctype",
    "operation",
    "actor",
    "on_behalf_of",
    "component",
    "path",
    "before",
    "after",
    "changed_fields",
    "command_id",
    "diff_digest",
    "graph",
    "edge_type",
    "properties",
    "field",
    "error",
    "method",
    "node",
    "error_class",
    "error_code",
    // 0.7.2: the nine topics of `system_platform`. Each is a name, an identifier, a digest or a
    // bounded value that `system_platform::check` validates; none is prose.
    "job",
    "args",
    "idempotency_key",
    "not_before",
    "payload",
    "name",
    "step",
    "class",
    "store_refs",
    "digest",
    "state",
    "series",
    "subject",
    "company",
    "policy",
    "reason",
    "detail",
    "case",
    "hash",
    "plan_digest",
    "id",
    "params",
    // 0.7.7: `queue` (1 to 64 bytes of `[a-z0-9_-]`) and `context` (an object, no float) of
    // `sys_job_enqueued`, both validated by `system_platform::check`.
    "queue",
    "context",
    // 0.7.6: the five journal and sync audit topics of `journal` (WP-JRN-01.T2). Each is a
    // bounded id, code or name; none is prose.
    "grant",
    "grantor",
    "target",
    "code",
    "entity",
    "fingerprint",
];

pub(crate) type Fields = BTreeMap<String, FieldSchema>;

pub(crate) fn req(fields: &mut Fields, name: &str, ty: PayloadType) {
    fields.insert(name.to_owned(), FieldSchema::required(ty));
}

pub(crate) fn opt(fields: &mut Fields, name: &str, ty: PayloadType) {
    fields.insert(name.to_owned(), FieldSchema::optional(ty));
}

pub(crate) fn text() -> PayloadType {
    PayloadType::Text
}

pub(crate) fn closed(values: &[&str]) -> PayloadType {
    PayloadType::Enum {
        values: values.iter().map(|v| (*v).to_owned()).collect(),
        open: false,
    }
}

fn open(values: &[&str]) -> PayloadType {
    PayloadType::Enum {
        values: values.iter().map(|v| (*v).to_owned()).collect(),
        open: true,
    }
}

fn node() -> PayloadType {
    let mut f = Fields::new();
    req(&mut f, "doctype", text());
    req(&mut f, "id", PayloadType::Id);
    PayloadType::Object(f)
}

/// The operations of an audit event that are a plain string; a whitelisted method is
/// `{"method": "<name>"}`.
pub const AUDIT_OPERATIONS: [&str; 7] = [
    "create", "update", "delete", "post", "cancel", "correct", "rename",
];

/// The two halves of a correction (ADR-42), the only values of `correction_half`. They accompany
/// operation `correct` and nothing else.
pub const CORRECTION_HALVES: [&str; 2] = ["cancel", "amend"];

/// How a write came about when the operation alone does not say (a closed set): `step` for a
/// write a lifecycle step made, `method` for a method write whose name the producer dropped.
pub const AUDIT_VIAS: [&str; 2] = ["step", "method"];

/// The outcome of a refused operation, the only value of `outcome`. A write that was made has no
/// outcome field: there is no `ok` here, because the written event *is* the write.
pub const AUDIT_DENIED: &str = "denied";

/// The `outcome` values an audit event may carry (a closed set of one).
pub const AUDIT_OUTCOMES: [&str; 1] = [AUDIT_DENIED];

/// The sources of an audit event (an open set).
pub const AUDIT_SOURCES: [&str; 5] = ["api", "job", "import", "sync", "script"];

/// The edge operations (a closed set: a new operation is a new major version).
pub const EDGE_OPS: [&str; 3] = ["add", "remove", "update"];

/// The secret operations, as `spot-document`'s `SecretOp::as_str` names them (a closed set:
/// a new operation is a new major version). `create_with_secrets` writes one `set` per field.
pub const SECRET_OPS: [&str; 5] = ["set", "open", "find", "rewrap", "is_set"];

/// Who held the secret capability (a closed set: the engine's `SecretCaller::as_str`).
/// `test` exists only in a build with the `testing` feature. `webhooks` (the webhook signing
/// secret's write path, WP-INT-01.T5) and `operator` (the `spot secrets` command line, whose key
/// rotation sweep used to be recorded as `seed`, WP-SEC-03.T12) were added in 0.7.5; the set
/// stays closed, and a further caller is a further additive amendment.
pub const SECRET_CALLERS: [&str; 5] = ["login_service", "seed", "webhooks", "operator", "test"];

/// The outcomes of a secret call (a closed set): the engine writes `ok`, or `refused` with the
/// code the call ended with. There is no third outcome: a call that fails is a refusal.
pub const SECRET_OUTCOMES: [&str; 2] = ["ok", "refused"];

/// What the engine records of the secret afterwards: `<set>`, `<unset>`, or `<none>` when the
/// call failed and nothing is known to have changed (the engine's `SET`, `UNSET`,
/// `NOT_RECORDED`).
pub const SECRET_STATES: [&str; 3] = ["<set>", "<unset>", "<none>"];

/// The `field` of one row of a rewrap batch: every secret field of that record.
pub const SECRET_REWRAP_FIELD: &str = "*";

/// The v1 payload schema of a `sys_*` topic registered here, or `None` for any other name.
///
/// The major version is 1 for every topic. The revision is in the registry only: the document
/// audit event's v1 payload gained the seven schema 2 fields at revision 1 (ADR-056 amendment
/// A6), the secret audit event's v1 payload gained two callers at revision 1 (amendment A7), and
/// every other v1 schema here is still at revision 0.
pub fn schema_of(topic: &str) -> Option<EventSchema> {
    let mut f = Fields::new();
    match topic {
        DOC_AUDIT_TOPIC => {
            req(&mut f, "site", text());
            req(&mut f, "doctype", text());
            req(&mut f, "id", PayloadType::Id);
            req(&mut f, "version", PayloadType::Int);
            // A string from `AUDIT_OPERATIONS` or `{"method": name}`; checked by `validate`.
            req(&mut f, "operation", PayloadType::Json);
            let mut actor = Fields::new();
            req(&mut actor, "actor", text());
            opt(&mut actor, "on_behalf_of", text());
            opt(&mut actor, "component", text());
            req(&mut f, "actor", PayloadType::Object(actor));
            // Unix microseconds.
            req(&mut f, "at", PayloadType::Int);
            req(
                &mut f,
                "changed_fields",
                PayloadType::List(Box::new(text())),
            );
            let mut change = Fields::new();
            req(&mut change, "path", text());
            opt(&mut change, "before", PayloadType::Json);
            opt(&mut change, "after", PayloadType::Json);
            req(
                &mut f,
                "diff",
                PayloadType::List(Box::new(PayloadType::Object(change))),
            );
            req(&mut f, "hidden_changes", PayloadType::Bool);
            opt(&mut f, "command_id", text());
            req(&mut f, "source", open(&AUDIT_SOURCES));
            req(&mut f, "truncated", PayloadType::Bool);
            opt(&mut f, "diff_digest", text());
            // Schema 2 (ADR-056 amendment A6). A method name, bounded by `validate`: 1 to
            // `MAX_METHOD_BYTES` bytes of `[a-z0-9_]`.
            opt(&mut f, "method", text());
            // One of `CORRECTION_HALVES`; only with operation `correct`.
            opt(&mut f, "correction_half", closed(&CORRECTION_HALVES));
            // One of `AUDIT_VIAS`; a closed set, never a free string.
            opt(&mut f, "via", closed(&AUDIT_VIAS));
            // A deployment node name; bounded by `validate`.
            opt(&mut f, "node", text());
            // One of `AUDIT_OUTCOMES` (`denied`), and the only way to say a write did not happen.
            opt(&mut f, "outcome", closed(&AUDIT_OUTCOMES));
            // The refusal's class and code; bounded catalogue codes, only with `denied`.
            opt(&mut f, "error_class", text());
            opt(&mut f, "error_code", text());
        }
        GRAPH_EDGE_TOPIC => {
            req(&mut f, "graph", text());
            req(&mut f, "op", closed(&EDGE_OPS));
            req(&mut f, "edge_type", text());
            req(&mut f, "from", node());
            req(&mut f, "to", node());
            opt(&mut f, "owner", node());
            opt(&mut f, "properties", PayloadType::Json);
            req(&mut f, "holder_version", PayloadType::Int);
        }
        SECRET_AUDIT_TOPIC => {
            req(&mut f, "op", closed(&SECRET_OPS));
            // A DocType name, never a value; checked by `validate`.
            req(&mut f, "doctype", text());
            // A secret field name or "*", never a value, a blob, an index or a hex form.
            req(&mut f, "field", text());
            // Absent when the call named no record.
            opt(&mut f, "record", PayloadType::Id);
            req(&mut f, "caller", closed(&SECRET_CALLERS));
            req(&mut f, "site", text());
            req(&mut f, "outcome", closed(&SECRET_OUTCOMES));
            // The code a refusal ended with; absent on success. Checked by `validate`.
            opt(&mut f, "error", text());
            req(&mut f, "state", closed(&SECRET_STATES));
        }
        _ => return platform::schema_of(topic).or_else(|| crate::journal::schema_of(topic)),
    }
    let topic = match Topic::any(topic) {
        Ok(t) => t,
        Err(e) => unreachable!("sys topic {topic} is invalid: {e}"),
    };
    let revision = audit_revision(&topic);
    Some(EventSchema {
        topic,
        major: 1,
        revision,
        fields: f,
        retired: BTreeSet::new(),
    })
}

/// The revision of a topic's v1 payload, in the registry only.
fn audit_revision(topic: &Topic) -> u32 {
    if topic.as_str() == DOC_AUDIT_TOPIC {
        // Schema 2's seven optional fields (ADR-056 amendment A6).
        DOC_AUDIT_REVISION
    } else if topic.as_str() == SECRET_AUDIT_TOPIC {
        // The `webhooks` and `operator` callers (ADR-056 amendment A7).
        SECRET_AUDIT_REVISION
    } else {
        0
    }
}

/// The v1 schemas of every `sys_*` topic registered here.
pub fn schemas() -> Vec<EventSchema> {
    TOPICS.iter().filter_map(|t| schema_of(t)).collect()
}

/// Validates a `sys_*` event's `data`: strictly against the topic's schema, then the structural
/// rules of its `Json` fields, the size bounds, and a scan of every key at every depth against
/// [`FORBIDDEN_KEY_PARTS`].
///
/// A topic that is not registered here is reported as an error on the empty path.
pub fn validate(topic: &str, data: &Json) -> Result<(), Vec<SchemaError>> {
    let Some(schema) = schema_of(topic) else {
        return Err(vec![err(
            "",
            format!("{topic} is not a registered sys topic"),
        )]);
    };
    let mut errors = schema.validate(data).err().unwrap_or_default();
    scan(data, "", &mut errors);
    if errors.is_empty() {
        match topic {
            DOC_AUDIT_TOPIC => check_audit(data, &mut errors),
            SECRET_AUDIT_TOPIC => check_secret_audit(data, &mut errors),
            GRAPH_EDGE_TOPIC => check_edge(data, &mut errors),
            other if crate::journal::TOPICS.contains(&other) => {
                crate::journal::check(other, data, &mut errors)
            }
            other => platform::check(other, data, &mut errors),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

pub(crate) fn err(path: &str, message: impl Into<String>) -> SchemaError {
    SchemaError {
        path: path.to_owned(),
        message: message.into(),
    }
}

/// Key scan and the generic string bound (a diff `text` value has its own bound, checked later,
/// so it is skipped here).
fn scan(v: &Json, path: &str, errors: &mut Vec<SchemaError>) {
    match v {
        Json::Object(map) => {
            for (k, child) in map {
                let here = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                let lower = k.to_ascii_lowercase();
                if FORBIDDEN_KEY_PARTS.iter().any(|p| lower.contains(p)) {
                    errors.push(err(
                        &here,
                        "a sys event never carries a credential or secret",
                    ));
                }
                // A diff value's `text` is bounded separately.
                if k == "text" && is_diff_side(path) {
                    continue;
                }
                scan(child, &here, errors);
            }
        }
        Json::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                scan(child, &format!("{path}[{i}]"), errors);
            }
        }
        Json::String(s) if s.chars().count() > MAX_STRING_CHARS => errors.push(err(
            path,
            format!("a sys event string is at most {MAX_STRING_CHARS} characters"),
        )),
        _ => {}
    }
}

fn is_diff_side(path: &str) -> bool {
    path.starts_with("diff[") && (path.ends_with(".before") || path.ends_with(".after"))
}

fn check_audit(data: &Json, errors: &mut Vec<SchemaError>) {
    let Some(obj) = data.as_object() else { return };
    match obj.get("operation") {
        Some(Json::String(s)) if AUDIT_OPERATIONS.contains(&s.as_str()) => {}
        Some(Json::Object(m)) if m.len() == 1 => match m.get("method") {
            Some(Json::String(n)) if !n.is_empty() && n.chars().count() <= 128 => {}
            _ => errors.push(err("operation", "a method name of 1 to 128 characters")),
        },
        _ => errors.push(err(
            "operation",
            "one of the operation names, or {\"method\": name}",
        )),
    }
    let operation = obj.get("operation");
    let operation_name = operation.and_then(Json::as_str);
    let operation_method = operation
        .and_then(Json::as_object)
        .and_then(|m| m.get("method"))
        .and_then(Json::as_str);
    check_audit_actor(obj.get("actor"), errors);
    if let Some(Json::Array(fields)) = obj.get("changed_fields")
        && fields.len() > MAX_CHANGED_FIELDS
    {
        errors.push(err(
            "changed_fields",
            format!("at most {MAX_CHANGED_FIELDS} names"),
        ));
    }
    if let Some(Json::Array(diff)) = obj.get("diff") {
        if diff.len() > MAX_DIFF_CHANGES {
            errors.push(err("diff", format!("at most {MAX_DIFF_CHANGES} entries")));
        }
        for (i, change) in diff.iter().enumerate() {
            for side in ["before", "after"] {
                if let Some(v) = change.get(side) {
                    check_diff_value(v, &format!("diff[{i}].{side}"), errors);
                }
            }
        }
    }
    let truncated = obj.get("truncated").and_then(Json::as_bool);
    let digest = obj.get("diff_digest").filter(|v| !v.is_null());
    match (truncated, digest) {
        (Some(true), None) => errors.push(err("diff_digest", "required when truncated")),
        (Some(false), Some(_)) => errors.push(err("diff_digest", "only present when truncated")),
        (_, Some(d)) if !d.as_str().is_some_and(is_hex_digest) => {
            errors.push(err("diff_digest", "64 lower-case hex characters"))
        }
        _ => {}
    }
    check_audit_schema2(obj, operation_name, operation_method, errors);
}

/// The seven optional fields of the audit participant's schema 2 (ADR-056 amendment A6) and the
/// rules that tie them to each other.
///
/// Every field here is written by a producer the platform cannot vouch for: a denial is caused by
/// anyone who can make a refused request. So each value is bounded, `outcome` is a closed set of
/// one, and a denial carries no diff, no changed field, no digest and no truncation — there is no
/// field for a message at all.
fn check_audit_schema2(
    obj: &serde_json::Map<String, Json>,
    operation_name: Option<&str>,
    operation_method: Option<&str>,
    errors: &mut Vec<SchemaError>,
) {
    // A method-made write names its method twice, and the two agree: the operation carries the
    // same bounded name, so the operation is never the only carrier of an unbounded string.
    match obj.get("method") {
        None | Some(Json::Null) => {}
        Some(Json::String(name)) if is_method_name(name) => {
            if operation_method != Some(name.as_str()) {
                errors.push(err(
                    "method",
                    "the method the operation names ({\"method\": name})",
                ));
            }
        }
        Some(_) => errors.push(err(
            "method",
            format!("1 to {MAX_METHOD_BYTES} bytes of [a-z0-9_]"),
        )),
    }
    // The half of a correction, and only then.
    if present(obj, "correction_half") && operation_name != Some("correct") {
        errors.push(err("correction_half", "only with operation \"correct\""));
    }
    // The deployment node (ADR-062) is a name, never a free string.
    match obj.get("node") {
        None | Some(Json::Null) => {}
        Some(Json::String(n)) if valid_property_name(n) => {}
        Some(_) => errors.push(err(
            "node",
            format!("a deployment node name, [a-z][a-z0-9_]*, at most {MAX_NAME_CHARS} bytes"),
        )),
    }
    // `outcome` has one value, so `denied` is exactly the denial. Tying the rule to the value
    // rather than to the field's presence keeps it right if a second outcome is ever added.
    let denied = obj.get("outcome").and_then(Json::as_str) == Some(AUDIT_DENIED);
    if denied {
        // A denial records the attempt, not a change: no diff, no changed field, no digest and
        // no truncation. A message has no field to arrive in.
        if obj
            .get("diff")
            .and_then(Json::as_array)
            .is_some_and(|d| !d.is_empty())
        {
            errors.push(err("diff", "a denial carries no diff"));
        }
        if obj
            .get("changed_fields")
            .and_then(Json::as_array)
            .is_some_and(|f| !f.is_empty())
        {
            errors.push(err("changed_fields", "a denial carries no changed fields"));
        }
        if obj.get("truncated").and_then(Json::as_bool) == Some(true) {
            errors.push(err("truncated", "a denial is never truncated"));
        }
        if obj.get("diff_digest").filter(|v| !v.is_null()).is_some() {
            errors.push(err("diff_digest", "a denial carries no digest"));
        }
    }
    // The refusal's class and code: catalogue codes, and only a denial has one.
    for field in ["error_class", "error_code"] {
        match obj.get(field) {
            None | Some(Json::Null) => {}
            Some(Json::String(code)) if is_bounded_code(code) => {
                if !denied {
                    errors.push(err(field, "only a denial carries a code"));
                }
            }
            Some(_) => errors.push(err(
                field,
                format!("a catalogue code, as namespace.reason, at most {MAX_NAME_CHARS} bytes"),
            )),
        }
    }
}

/// Whether an optional field carries a value (absent and `null` are the same thing).
pub(crate) fn present(obj: &serde_json::Map<String, Json>, field: &str) -> bool {
    obj.get(field).is_some_and(|v| !v.is_null())
}

/// A method name as the audit records it: 1 to [`MAX_METHOD_BYTES`] bytes of `[a-z0-9_]` (the
/// producer's `is_recordable_method`, which drops a name that is not of that shape).
fn is_method_name(name: &str) -> bool {
    (1..=MAX_METHOD_BYTES).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A catalogue code as a denial carries it: lower-case `namespace.reason` segments separated by
/// dots, at most [`MAX_NAME_CHARS`] characters in all. Never a message, never a value.
///
/// The segment count is not fixed: the catalogue already has two-, three- and four-segment codes
/// (`offline_identity.quarantine.tier1_after_revocation`), and the total bound is what stops a
/// message from being carried in the field.
fn is_bounded_code(code: &str) -> bool {
    code.len() <= MAX_NAME_CHARS && code.split('.').all(valid_property_name)
}

/// The audit actor is a typed principal reference (`user:<id>`, `consumer:<id>`) or `system`
/// (ADR-083 section 2.2 item 4); `on_behalf_of`, the real person behind a user, is a `user:`
/// reference only: a consumer acts as itself and is never impersonated.
fn check_audit_actor(actor: Option<&Json>, errors: &mut Vec<SchemaError>) {
    let Some(actor) = actor.and_then(Json::as_object) else {
        return;
    };
    match actor.get("actor").and_then(Json::as_str) {
        Some(SYSTEM_PRINCIPAL) => {}
        Some(text) if text.parse::<PrincipalRef>().is_ok() => {}
        Some(_) => errors.push(err(
            concat!("actor", ".actor"),
            "`user:<id>`, `consumer:<id>` or `system`",
        )),
        None => {}
    }
    match actor.get("on_behalf_of") {
        None | Some(Json::Null) => {}
        Some(Json::String(text))
            if text
                .parse::<PrincipalRef>()
                .is_ok_and(|p| p.kind() == PrincipalKind::User)
                && actor
                    .get("actor")
                    .and_then(Json::as_str)
                    .and_then(|a| a.parse::<PrincipalRef>().ok())
                    .is_some_and(|a| a.kind() == PrincipalKind::User) => {}
        Some(_) => errors.push(err(
            concat!("actor", ".on_behalf_of"),
            "a `user:<id>` reference, and only on a `user:` actor",
        )),
    }
}

pub(crate) fn is_hex_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `"null"`, `{"bool": b}`, `{"int": n}`, `{"text": s}` or `{"digest": {"len", "digest"}}`.
fn check_diff_value(v: &Json, path: &str, errors: &mut Vec<SchemaError>) {
    match v {
        Json::Null => {}
        Json::String(s) if s == "null" => {}
        Json::Object(m) if m.len() == 1 => {
            let (tag, inner) = m
                .iter()
                .next()
                .map_or(("", &Json::Null), |(k, v)| (k.as_str(), v));
            let ok = match tag {
                "bool" => inner.is_boolean(),
                "int" => inner.is_i64(),
                "text" => inner
                    .as_str()
                    .is_some_and(|s| s.chars().count() <= MAX_DIFF_TEXT_CHARS),
                "digest" => inner.as_object().is_some_and(|d| {
                    d.len() == 2
                        && d.get("len").is_some_and(Json::is_u64)
                        && d.get("digest")
                            .and_then(Json::as_str)
                            .is_some_and(is_hex_digest)
                }),
                _ => false,
            };
            if !ok {
                errors.push(err(
                    path,
                    format!(
                        "a bad diff {tag} value (text is at most {MAX_DIFF_TEXT_CHARS} characters)"
                    ),
                ));
            }
        }
        _ => errors.push(err(
            path,
            "a diff value is \"null\", {bool}, {int}, {text} or {digest}",
        )),
    }
}

fn check_edge(data: &Json, errors: &mut Vec<SchemaError>) {
    let Some(obj) = data.as_object() else { return };
    let remove = obj.get("op").and_then(Json::as_str) == Some("remove");
    match obj.get("properties") {
        None | Some(Json::Null) => {}
        Some(Json::Object(props)) => {
            if remove && !props.is_empty() {
                errors.push(err("properties", "a remove event carries no properties"));
            }
            if props.len() > MAX_EDGE_PROPERTIES {
                errors.push(err(
                    "properties",
                    format!("at most {MAX_EDGE_PROPERTIES} properties"),
                ));
            }
            for (name, value) in props {
                let path = format!("properties.{name}");
                if !valid_property_name(name) {
                    errors.push(err(
                        &path,
                        "property names are [a-z][a-z0-9_]*, at most 63 bytes",
                    ));
                }
                check_property_value(value, &path, errors);
            }
        }
        Some(_) => errors.push(err("properties", "an object of typed values")),
    }
}

pub(crate) fn valid_property_name(n: &str) -> bool {
    let mut b = n.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_lowercase())
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && n.len() <= MAX_NAME_CHARS
}

/// A DocType name or a secret field name: a name as the platform writes it, never a value.
///
/// [`valid_property_name`] already refuses spaces, punctuation, upper case and anything longer
/// than [`MAX_NAME_CHARS`]. Two more shapes are refused because only a secret has them:
/// a derived column ([`has_derived_suffix`]: the sealed blob or the blind index of a field) and
/// a hex run of 16 characters or more ([`has_hex_run`]: a value's hex form or a digest).
fn is_secret_name(n: &str) -> bool {
    valid_property_name(n) && !has_derived_suffix(n) && !has_hex_run(n)
}

/// Whether a name ends in a derived-column suffix: `__sealed` or `__bidx` (see `spot-meta`).
fn has_derived_suffix(n: &str) -> bool {
    n.ends_with("__sealed") || n.ends_with("__bidx")
}

/// Whether a name holds a run of 16 or more lower-case hex characters between underscores.
fn has_hex_run(n: &str) -> bool {
    n.split('_').any(|part| {
        part.len() >= 16
            && part
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// A catalogue code, as an `error` field carries it: two or three lower-case segments separated
/// by dots (`secretfield.open_failed`, `document.version_conflict`), at most
/// [`MAX_NAME_CHARS`] characters. Never a message, never a value.
fn is_catalogue_code(code: &str) -> bool {
    if code.len() > MAX_NAME_CHARS {
        return false;
    }
    let segments: Vec<&str> = code.split('.').collect();
    (2..=3).contains(&segments.len()) && segments.iter().all(|s| valid_property_name(s))
}

fn check_secret_audit(data: &Json, errors: &mut Vec<SchemaError>) {
    let Some(obj) = data.as_object() else { return };
    // A DocType and a field are names here; the event reports which secret was touched, and
    // nothing of it. A field name may be "*", the rewrap batch row.
    match obj.get("doctype") {
        Some(Json::String(s)) if is_secret_name(s) => {}
        _ => errors.push(err("doctype", "a DocType name, never a value")),
    }
    match obj.get("field") {
        Some(Json::String(s)) if s == SECRET_REWRAP_FIELD || is_secret_name(s) => {}
        _ => errors.push(err(
            "field",
            "a secret field name or \"*\", never a value, a sealed blob, a blind index or a hex form",
        )),
    }
    // The outcome and the code agree: a refusal names the code it ended with, a success none.
    let error = obj.get("error").filter(|v| !v.is_null());
    match (
        obj.get("outcome").and_then(Json::as_str) == Some("refused"),
        error,
    ) {
        (true, Some(Json::String(code))) if is_catalogue_code(code) => {}
        (true, _) => errors.push(err(
            "error",
            "a refusal names its code, as namespace.reason",
        )),
        (false, None) => {}
        (false, Some(_)) => errors.push(err("error", "only a refusal carries a code")),
    }
}

/// `null` or one tagged value: `bool`, `int`, `decimal`, `text`, `date`, `time`, `datetime`, `id`.
/// There is no float and no nested JSON: an emitter projects a JSON or byte value onto a text
/// or a digest before writing the event.
fn check_property_value(v: &Json, path: &str, errors: &mut Vec<SchemaError>) {
    let Json::Object(m) = v else {
        if !v.is_null() {
            errors.push(err(path, "null or a tagged value such as {\"int\": 3}"));
        }
        return;
    };
    let Some((tag, inner)) = (m.len() == 1).then(|| m.iter().next()).flatten() else {
        errors.push(err(path, "exactly one tag per value"));
        return;
    };
    let s = inner.as_str();
    let ok = match tag.as_str() {
        "bool" => inner.is_boolean(),
        "int" => inner.is_i64(),
        "decimal" => s.is_some_and(crate::schema::is_decimal),
        "text" => s.is_some(),
        "date" => s.is_some_and(is_date),
        "time" => s.is_some_and(is_time),
        "datetime" => s.is_some_and(is_utc_datetime),
        "id" => s.is_some_and(|x| crate::payload::record_id::parse(x).is_ok()),
        _ => false,
    };
    if !ok {
        errors.push(err(path, format!("not a valid {tag} value")));
    }
}

/// `HH:MM:SS` with an optional fraction.
fn is_time(s: &str) -> bool {
    let (clock, frac) = match s.split_once('.') {
        Some((c, f)) => (c, Some(f)),
        None => (s, None),
    };
    let parts: Vec<&str> = clock.split(':').collect();
    let num = |p: &str, max: u8| {
        p.len() == 2
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u8>().is_ok_and(|n| n <= max)
    };
    parts.len() == 3
        && num(parts[0], 23)
        && num(parts[1], 59)
        && num(parts[2], 59)
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}
