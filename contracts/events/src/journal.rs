//! The platform's journal and sync audit topics (ADR-061 sections 7 and 8 and the answer to
//! question 5; ADR-067; ADR-062 section 3; WP-JRN-01.T2).
//!
//! Hosting the journal (WP-JRN-09) and sync (the WP-RTB-01 to WP-RTB-04 family, WP-JRN-09) adds
//! five operations that change who can read a tenant's whole history, move its data or stop a
//! node. Every one of them, success, refusal and failure, is written as a class **Audit**
//! event, following the pattern already set by `identity_*` and `sys_doc_audit`: who, what,
//! outcome, never content, never a key or a credential.
//!
//! | Topic | What it records |
//! | --- | --- |
//! | [`JOURNAL_ACCESS_TOPIC`] | One read of journal bytes through the access surface of WP-JRN-09.T4. |
//! | [`JOURNAL_GRANT_TOPIC`] | The lifecycle of a `journal_reader` grant (ADR-061 section 7 item 2, question 5). |
//! | [`JOURNAL_ADMIN_TOPIC`] | An operation on the journal itself: policy, snapshot, rebuild, fork, audit, reship, key rotation, erasure. |
//! | [`SYNC_NODE_ADMIN_TOPIC`] | A sync node's lifecycle: enrolment, certificate rotation, suspension, role change (ADR-062 section 3). |
//! | [`SYNC_DECISION_TOPIC`] | A sync decision an operator or a rule made on data (ADR-067). |
//!
//! Same mechanism as [`crate::system_platform`]: a strict [`EventSchema`], then the structural
//! rules below, run by [`crate::system::validate`] after its generic key scan (no field named
//! like a secret at any depth) and its 256-character string bound. None of these five payloads
//! carries a key, a credential, a record id list or a document's content: `principal`,
//! `grantee` and `grant` (when it names a grant) are typed record ids; `grantor` and `actor`
//! are a principal id or the literal `system`; `target` is a position, a policy name or a
//! snapshot id and is refused when it parses as a record id; `code` is a bounded catalogue code,
//! allowed only when the outcome is not `ok`.

use std::collections::BTreeSet;

use serde_json::{Map, Value as Json};

use crate::names::Topic;
use crate::payload::SYSTEM_PRINCIPAL;
use crate::schema::{EventSchema, PayloadType, SchemaError};
use crate::system::{
    Fields, MAX_NAME_CHARS, closed, err, is_hex_digest, opt, present, req, text,
    valid_property_name,
};

/// The topic of one read of journal bytes.
pub const JOURNAL_ACCESS_TOPIC: &str = "sys_journal_access";
/// The topic of a `journal_reader` grant's lifecycle.
pub const JOURNAL_GRANT_TOPIC: &str = "sys_journal_grant";
/// The topic of an operation on the journal itself.
pub const JOURNAL_ADMIN_TOPIC: &str = "sys_journal_admin";
/// The topic of a sync node's lifecycle.
pub const SYNC_NODE_ADMIN_TOPIC: &str = "sys_sync_node_admin";
/// The topic of a sync decision made on data.
pub const SYNC_DECISION_TOPIC: &str = "sys_sync_decision";

/// Every topic this module registers a payload schema for.
pub const TOPICS: [&str; 5] = [
    JOURNAL_ACCESS_TOPIC,
    JOURNAL_GRANT_TOPIC,
    JOURNAL_ADMIN_TOPIC,
    SYNC_NODE_ADMIN_TOPIC,
    SYNC_DECISION_TOPIC,
];

/// The literal `sys_journal_access.grant` may hold instead of a grant id: access made as the
/// tenant administrator, with no `journal_reader` grant to name.
pub const ACCESS_GRANT_ADMIN: &str = "admin";

/// What `sys_journal_access.object` names.
pub const ACCESS_OBJECTS: [&str; 4] = ["manifest", "segment", "tail_chunk", "snapshot_index"];
/// The outcomes of a journal access attempt.
pub const ACCESS_OUTCOMES: [&str; 5] = ["served", "denied", "not_found", "expired", "failed"];

/// The actions of a `journal_reader` grant's lifecycle.
pub const GRANT_ACTIONS: [&str; 5] = ["granted", "renewed", "revoked", "expired", "refused"];

/// The operations `sys_journal_admin` records.
pub const ADMIN_ACTIONS: [&str; 13] = [
    "policy_changed",
    "snapshot_taken",
    "snapshot_failed",
    "rebuild_planned",
    "rebuild_started",
    "rebuild_completed",
    "rebuild_failed",
    "fork_opened",
    "audit_run",
    "reship_started",
    "reship_completed",
    "key_rotated",
    "subject_erased",
];
/// The outcomes of an admin operation (`sys_journal_admin`, `sys_sync_node_admin` and
/// `sys_sync_decision` share this closed set).
pub const ADMIN_OUTCOMES: [&str; 3] = ["ok", "refused", "failed"];

/// The actions of a sync node's lifecycle.
pub const NODE_ADMIN_ACTIONS: [&str; 6] = [
    "enrol_started",
    "enrolled",
    "certificate_rotated",
    "suspended",
    "resumed",
    "revoked",
];
// `role_changed` is a seventh action (ADR-062 section 3): a node's role may change without a
// lifecycle transition. Kept in its own constant so the closed set reads as one addition.
const NODE_ROLE_CHANGED: &str = "role_changed";

/// The actions of a sync decision.
pub const DECISION_ACTIONS: [&str; 8] = [
    "resend_requested",
    "reship_requested",
    "contact_notified",
    "quarantine_released",
    "quarantine_discarded",
    "hold_released",
    "adoption_forced",
    "policy_applied",
];

/// The v1 payload schema of one of the five topics, or `None` for any other name.
pub fn schema_of(topic: &str) -> Option<EventSchema> {
    let mut f = Fields::new();
    match topic {
        JOURNAL_ACCESS_TOPIC => {
            req(&mut f, "principal", PayloadType::Id);
            // The journal_reader grant id, or the literal "admin"; checked by `check`.
            req(&mut f, "grant", text());
            req(&mut f, "object", closed(&ACCESS_OBJECTS));
            req(&mut f, "generation", PayloadType::Int);
            // Required unless object is manifest; checked by `check`.
            opt(&mut f, "first_seq", PayloadType::Int);
            opt(&mut f, "last_seq", PayloadType::Int);
            req(&mut f, "outcome", closed(&ACCESS_OUTCOMES));
            opt(&mut f, "reason", text());
            opt(&mut f, "bytes", PayloadType::Int);
        }
        JOURNAL_GRANT_TOPIC => {
            req(&mut f, "action", closed(&GRANT_ACTIONS));
            req(&mut f, "grantee", PayloadType::Id);
            // A principal id, or "system" for an expiry; checked by `check`.
            req(&mut f, "grantor", text());
            req(&mut f, "grant", PayloadType::Id);
            // Required for granted and renewed only; checked by `check`.
            opt(&mut f, "expires_at", PayloadType::DateTime);
            req(&mut f, "external", PayloadType::Bool);
            opt(&mut f, "reason", text());
        }
        JOURNAL_ADMIN_TOPIC => {
            req(&mut f, "action", closed(&ADMIN_ACTIONS));
            // A principal id, or "system"; checked by `check`.
            req(&mut f, "actor", text());
            // A position (generation:seq), a policy name or a snapshot id; never a record id or
            // content. Checked by `check`.
            opt(&mut f, "target", text());
            req(&mut f, "outcome", closed(&ADMIN_OUTCOMES));
            opt(&mut f, "code", text());
            req(&mut f, "dry_run", PayloadType::Bool);
        }
        SYNC_NODE_ADMIN_TOPIC => {
            req(&mut f, "action", node_admin_actions());
            req(&mut f, "node", text());
            req(&mut f, "actor", text());
            req(&mut f, "outcome", closed(&ADMIN_OUTCOMES));
            opt(&mut f, "code", text());
            opt(&mut f, "fingerprint", text());
        }
        SYNC_DECISION_TOPIC => {
            req(&mut f, "action", closed(&DECISION_ACTIONS));
            opt(&mut f, "node", text());
            opt(&mut f, "entity", text());
            opt(&mut f, "count", PayloadType::Int);
            req(&mut f, "actor", text());
            req(&mut f, "outcome", closed(&ADMIN_OUTCOMES));
            opt(&mut f, "code", text());
        }
        _ => return None,
    }
    let topic = match Topic::any(topic) {
        Ok(t) => t,
        Err(e) => unreachable!("sys topic {topic} is invalid: {e}"),
    };
    Some(EventSchema {
        topic,
        major: 1,
        revision: 0,
        fields: f,
        retired: BTreeSet::new(),
    })
}

/// The closed set of node admin actions: [`NODE_ADMIN_ACTIONS`] plus [`NODE_ROLE_CHANGED`].
fn node_admin_actions() -> PayloadType {
    let mut values: Vec<&str> = NODE_ADMIN_ACTIONS.to_vec();
    values.push(NODE_ROLE_CHANGED);
    closed(&values)
}

/// The structural rules of the five topics, run by
/// [`system::validate`](crate::system::validate) after the strict schema passed.
pub(crate) fn check(topic: &str, data: &Json, errors: &mut Vec<SchemaError>) {
    let Some(obj) = data.as_object() else { return };
    match topic {
        JOURNAL_ACCESS_TOPIC => check_access(obj, errors),
        JOURNAL_GRANT_TOPIC => check_grant(obj, errors),
        JOURNAL_ADMIN_TOPIC => check_admin(obj, errors),
        SYNC_NODE_ADMIN_TOPIC => check_node_admin(obj, errors),
        SYNC_DECISION_TOPIC => check_decision(obj, errors),
        _ => {}
    }
}

fn int(obj: &Map<String, Json>, field: &str) -> Option<i64> {
    obj.get(field).and_then(Json::as_i64)
}

/// A short lower-case code: `[a-z][a-z0-9_]*`, at most 64 bytes. Never a message.
fn is_short_code(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// A catalogue code as `code` carries it: lower-case dot-separated segments, at most
/// [`MAX_NAME_CHARS`] characters in all. Never a message.
fn is_bounded_code(code: &str) -> bool {
    code.len() <= MAX_NAME_CHARS && code.split('.').all(valid_property_name)
}

/// Whether `s` parses as a canonical record id (used to refuse one where it must never appear).
fn is_record_id(s: &str) -> bool {
    crate::payload::record_id::parse(s).is_ok()
}

/// `s` is `literal`, or a canonical record id.
fn id_or_literal(s: &str, literal: &str) -> bool {
    s == literal || is_record_id(s)
}

/// A principal id, or the platform itself (`system`).
fn is_actor(s: &str) -> bool {
    id_or_literal(s, SYSTEM_PRINCIPAL)
}

/// A node id as the sync protocol writes it: a lower-case host name, optionally `#` and a
/// storage name (the same shape `system_platform::is_node_id` checks for `sys_partition_range_low`).
fn is_node_id(s: &str) -> bool {
    let (site, partition) = match s.split_once('#') {
        Some((site, partition)) => (site, Some(partition)),
        None => (s, None),
    };
    let host = !site.is_empty()
        && site.len() <= 253
        && site.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    host && partition.is_none_or(valid_property_name)
}

/// `code` is only present when the outcome is not `ok` (the pattern of `sys_doc_audit`'s
/// `error_class`/`error_code`, which only a denial carries).
fn check_code(obj: &Map<String, Json>, outcome: Option<&str>, errors: &mut Vec<SchemaError>) {
    match obj.get("code") {
        None | Some(Json::Null) => {}
        Some(Json::String(c)) if is_bounded_code(c) => {
            if outcome == Some("ok") {
                errors.push(err("code", "only present when the outcome is not ok"));
            }
        }
        Some(_) => errors.push(err("code", "a catalogue code, as namespace.reason")),
    }
}

fn check_access(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    match obj.get("grant").and_then(Json::as_str) {
        Some(g) if id_or_literal(g, ACCESS_GRANT_ADMIN) => {}
        _ => errors.push(err("grant", "the journal_reader grant id, or \"admin\"")),
    }
    let is_manifest = obj.get("object").and_then(Json::as_str) == Some("manifest");
    let first = int(obj, "first_seq");
    let last = int(obj, "last_seq");
    if !is_manifest && (first.is_none() || last.is_none()) {
        errors.push(err("first_seq", "required unless object is manifest"));
    }
    if let (Some(f), Some(l)) = (first, last) {
        if f < 0 || l < 0 || l < f {
            errors.push(err("last_seq", "not negative, and not less than first_seq"));
        }
    } else if first.is_some() != last.is_some() {
        errors.push(err("last_seq", "first_seq and last_seq are given together"));
    }
    if int(obj, "generation").is_some_and(|g| g < 0) {
        errors.push(err("generation", "not negative"));
    }
    if int(obj, "bytes").is_some_and(|b| b < 0) {
        errors.push(err("bytes", "not negative"));
    }
    if let Some(Json::String(r)) = obj.get("reason")
        && !is_short_code(r)
    {
        errors.push(err("reason", "a short lower-case code"));
    }
}

fn check_grant(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    let action = obj.get("action").and_then(Json::as_str);
    let grantor = obj.get("grantor").and_then(Json::as_str);
    match grantor {
        Some(g) if id_or_literal(g, SYSTEM_PRINCIPAL) => {}
        _ => errors.push(err(
            "grantor",
            "a principal id, or \"system\" for an expired grant",
        )),
    }
    // An expiry is recorded by the platform, and only an expiry is: the literal is reserved for
    // that one action, so a grantor text never says who the automatic clock was.
    let is_system = grantor == Some(SYSTEM_PRINCIPAL);
    let is_expired = action == Some("expired");
    if is_system != is_expired {
        errors.push(err(
            "grantor",
            "\"system\" exactly when the action is expired",
        ));
    }
    let needs_expiry = matches!(action, Some("granted") | Some("renewed"));
    let has_expiry = present(obj, "expires_at");
    if needs_expiry && !has_expiry {
        errors.push(err("expires_at", "required when granted or renewed"));
    }
    if !needs_expiry && has_expiry {
        errors.push(err("expires_at", "only present when granted or renewed"));
    }
    if let Some(Json::String(r)) = obj.get("reason")
        && !is_short_code(r)
    {
        errors.push(err("reason", "a short lower-case code"));
    }
}

fn check_admin(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    if !obj
        .get("actor")
        .and_then(Json::as_str)
        .is_some_and(is_actor)
    {
        errors.push(err("actor", "a principal id, or \"system\""));
    }
    if let Some(Json::String(t)) = obj.get("target")
        && (t.is_empty() || is_record_id(t))
    {
        errors.push(err(
            "target",
            "a position, a policy name or a snapshot id; never a record id",
        ));
    }
    let outcome = obj.get("outcome").and_then(Json::as_str);
    check_code(obj, outcome, errors);
}

fn check_node_admin(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    match obj.get("node").and_then(Json::as_str) {
        Some(n) if is_node_id(n) => {}
        _ => errors.push(err("node", "a node id: <host>[#<partition>]")),
    }
    if !obj
        .get("actor")
        .and_then(Json::as_str)
        .is_some_and(is_actor)
    {
        errors.push(err("actor", "a principal id, or \"system\""));
    }
    let outcome = obj.get("outcome").and_then(Json::as_str);
    check_code(obj, outcome, errors);
    if let Some(Json::String(fp)) = obj.get("fingerprint")
        && !is_hex_digest(fp)
    {
        errors.push(err("fingerprint", "64 lower-case hex characters"));
    }
}

fn check_decision(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    if let Some(Json::String(n)) = obj.get("node")
        && !is_node_id(n)
    {
        errors.push(err("node", "a node id: <host>[#<partition>]"));
    }
    if let Some(Json::String(e)) = obj.get("entity")
        && !valid_property_name(e)
    {
        errors.push(err("entity", "a DocType name"));
    }
    if int(obj, "count").is_some_and(|c| c < 0) {
        errors.push(err("count", "not negative"));
    }
    if !obj
        .get("actor")
        .and_then(Json::as_str)
        .is_some_and(is_actor)
    {
        errors.push(err("actor", "a principal id, or \"system\""));
    }
    let outcome = obj.get("outcome").and_then(Json::as_str);
    check_code(obj, outcome, errors);
}
