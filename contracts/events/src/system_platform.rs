//! Payload schemas for the nine platform `sys_*` topics that 0.7.1 registered without one
//! (WP-CON-04.T6, events 0.7.2).
//!
//! Every schema here is **v1, revision 0**, uses the same mechanism as [`crate::system`]
//! (strict [`EventSchema`], then the structural rules of its `Json` fields, the key scan and the
//! 256-character string bound of [`system::validate`](crate::system::validate)), and is written
//! from what the producing crate puts in the outbox, not from what a consumer would like:
//!
//! | Topic | Producer | Payload (`data`) |
//! | --- | --- | --- |
//! | [`JOB_ENQUEUED_TOPIC`] | `spot-document` | `job`, `args`, `idempotency_key`, `not_before` |
//! | [`USER_NOTICE_TOPIC`] | `spot-document` | `user`, `payload` |
//! | [`OPERATION_RECEIPT_TOPIC`] | `spot-document` | the operation receipt, plus `undeclared` |
//! | [`OPERATION_RECEIPT_EFFECT_TOPIC`] | `spot-document` | `operation`, `effect`, `undeclared` |
//! | [`PARTITION_RANGE_LOW_TOPIC`] | `spot-partition` | `series`, `node`, `remaining`, `total` |
//! | [`BPM_GOVERNANCE_TOPIC`] | `spot-bpm-governance` | a serialised `spot-audit-chain` record |
//! | [`BPM_CASE_MIGRATED_TOPIC`] | `spot-bpm-versioning` | the case, both versions, the digest |
//! | [`CONDITION_RAISED_TOPIC`] | `spot-conditions` | a raised or escalated transition |
//! | [`CONDITION_CLEARED_TOPIC`] | `spot-conditions` | a cleared transition |
//!
//! # What each payload never carries
//!
//! Like the three older `sys_*` payloads, none of these carries a credential, a secret or a field
//! value of a document: [`system::validate`](crate::system::validate) refuses a key containing a
//! [`FORBIDDEN_KEY_PARTS`](crate::identity::FORBIDDEN_KEY_PARTS) entry at any depth and a string
//! of more than [`MAX_STRING_CHARS`](crate::system::MAX_STRING_CHARS) characters. The two payloads
//! an app fills (`args` of a job, `payload` of a notice) are app data: the contract bounds their
//! shape and refuses a floating-point number (ADR-12), and the producer owns their content.
//!
//! # Where the wire form is only inferred
//!
//! `sys_bpm_case_migrated` has no producer that writes it to the outbox yet: the crate builds a
//! `MigrationAudit` value and hands it to the host's `MigrationTarget`, which enqueues it. The
//! payload below is that value's fields in the platform's wire encoding (microseconds for time,
//! a record of the two version pins). It is the one schema of the nine that is not a copy of an
//! existing serialisation; see the task report.

use std::collections::BTreeSet;

use serde_json::{Map, Value as Json};
use time::format_description::well_known::Rfc3339;

use crate::names::Topic;
use crate::payload::{PrincipalKind, PrincipalRef};
use crate::schema::{EventSchema, PayloadType, SchemaError};
use crate::system::{
    Fields, closed, err, is_hex_digest, opt, present, req, text, valid_property_name,
};

/// The topic of an enqueued job (`spot-document`, read by the job intake).
pub const JOB_ENQUEUED_TOPIC: &str = "sys_job_enqueued";
/// The topic of a realtime notice to one user (`spot-document`, read by the realtime bridge).
pub const USER_NOTICE_TOPIC: &str = "sys_user_notice";
/// The topic of an operation's receipt (`spot-document`).
pub const OPERATION_RECEIPT_TOPIC: &str = "sys_operation_receipt";
/// The topic of one external effect announced after the commit (`spot-document`).
pub const OPERATION_RECEIPT_EFFECT_TOPIC: &str = "sys_operation_receipt_effect";
/// The topic of a branch series running low (`spot-partition`).
pub const PARTITION_RANGE_LOW_TOPIC: &str = "sys_partition_range_low";
/// The topic of a process governance audit entry (`spot-bpm-governance`).
pub const BPM_GOVERNANCE_TOPIC: &str = "sys_bpm_governance";
/// The topic of a migrated process Case (`spot-bpm-versioning`).
pub const BPM_CASE_MIGRATED_TOPIC: &str = "sys_bpm_case_migrated";
/// The topic of a raised or escalated condition (`spot-conditions`).
pub const CONDITION_RAISED_TOPIC: &str = "sys_condition_raised";
/// The topic of a cleared condition (`spot-conditions`).
pub const CONDITION_CLEARED_TOPIC: &str = "sys_condition_cleared";

/// The largest compact-JSON size of a notice `payload`, in bytes (the realtime bridge's
/// `NOTICE_MAX_BYTES`, ADR-25).
pub const NOTICE_MAX_BYTES: usize = 2 * 1024;
/// The longest key or string value of a notice `payload`, in bytes (the bridge's
/// `NOTICE_MAX_TEXT_BYTES`).
pub const NOTICE_MAX_TEXT_BYTES: usize = 256;

/// The lifecycle operations a receipt names, as `spot-app-api`'s `op_name` spells them (a closed
/// set: a new operation is a new major version).
pub const RECEIPT_OPERATIONS: [&str; 7] = [
    "create",
    "update",
    "post",
    "cancel",
    "delete",
    "rename",
    "update_posted",
];
/// Whether an effect stays inside the transaction or leaves it (a closed set).
pub const EFFECT_KINDS: [&str; 2] = ["internal", "external"];
/// The states of an effect that carry no data; a failed effect is `{"failed": {"code": ...}}`.
pub const EFFECT_STATES: [&str; 4] = ["applied", "held", "released", "delivered"];
/// Most effect entries one receipt holds (the engine's `MAX_EFFECTS_PER_RECEIPT`).
pub const MAX_EFFECTS_PER_RECEIPT: usize = 256;
/// Most opaque references all the entries of one receipt hold together (the engine's
/// `MAX_REFS_PER_RECEIPT`).
pub const MAX_REFS_PER_RECEIPT: usize = 4096;
/// The longest opaque reference, in bytes (`OpaqueRef::MAX_LEN`).
pub const MAX_REF_BYTES: usize = 128;
/// The longest failure code a receipt keeps (the engine's `MAX_CODE_LEN`).
pub const MAX_FAILURE_CODE_BYTES: usize = 64;
/// The longest series key, in bytes (`SeriesKey::MAX_LEN`).
pub const MAX_SERIES_BYTES: usize = 140;
/// The longest condition id, in bytes (`ConditionId::MAX_LEN`).
pub const MAX_CONDITION_ID_BYTES: usize = 64;
/// The most parameters a condition event may carry.
pub const MAX_CONDITION_PARAMS: usize = 64;

/// The kinds of a governance audit entry, as the producer's `AuditKind::as_str` spells them.
pub const GOVERNANCE_KINDS: [&str; 3] = [
    concat!("bpm.governance.", "outside_process"),
    concat!("bpm.governance.", "refused"),
    concat!("bpm.governance.", "policy_change"),
];
/// The one `policy` a governance entry names (the producer's `POLICY_KEY`).
pub const GOVERNANCE_POLICY: &str = concat!("bpm.governance.", "policy");
/// The modes a governance entry records (the producer's `GovernanceMode::as_str`).
pub const GOVERNANCE_MODES: [&str; 3] = ["free", "governed", "process_only"];
/// The keys of a governance entry's `payload`, the only ones the producer writes.
const GOVERNANCE_PAYLOAD_KEYS: [&str; 7] = [
    "company",
    "mode",
    "policy",
    "doctype",
    "reason",
    "detail",
    "command_id",
];

/// The transitions a `sys_condition_raised` event records (an escalation is a raise).
pub const RAISED_TRANSITIONS: [&str; 2] = ["raised", "escalated"];
/// The transition a `sys_condition_cleared` event records.
pub const CLEARED_TRANSITIONS: [&str; 1] = ["cleared"];
/// The severities of a condition (ADR-067 section 1), least severe first.
pub const SEVERITIES: [&str; 3] = ["info", "amber", "red"];

/// The v1 payload schema of one of the nine topics, or `None` for any other name.
pub fn schema_of(topic: &str) -> Option<EventSchema> {
    let mut f = Fields::new();
    match topic {
        JOB_ENQUEUED_TOPIC => {
            // `app:job`; checked by `check`.
            req(&mut f, "job", text());
            // Arguments in the native value encoding; absent when the job has none.
            opt(&mut f, "args", PayloadType::Json);
            opt(&mut f, "idempotency_key", text());
            // RFC 3339, with the offset the requester gave; checked by `check`.
            opt(&mut f, "not_before", text());
        }
        USER_NOTICE_TOPIC => {
            req(&mut f, "user", PayloadType::Id);
            // An object of scalars and arrays of scalars; checked by `check`.
            req(&mut f, "payload", PayloadType::Json);
        }
        OPERATION_RECEIPT_TOPIC => {
            req(&mut f, "operation", closed(&RECEIPT_OPERATIONS));
            req(&mut f, "doctype", text());
            req(&mut f, "name", PayloadType::Id);
            req(&mut f, "version", PayloadType::Int);
            req(
                &mut f,
                "effects",
                PayloadType::List(Box::new(PayloadType::Object(effect_fields()))),
            );
            // Unix microseconds.
            req(&mut f, "committed_at", PayloadType::Int);
            // Indexes into `effects` of the entries no declaration covers; absent when none.
            opt(
                &mut f,
                "undeclared",
                PayloadType::List(Box::new(PayloadType::Int)),
            );
        }
        OPERATION_RECEIPT_EFFECT_TOPIC => {
            req(&mut f, "operation", closed(&RECEIPT_OPERATIONS));
            req(&mut f, "effect", PayloadType::Object(effect_fields()));
            req(&mut f, "undeclared", PayloadType::Bool);
        }
        PARTITION_RANGE_LOW_TOPIC => {
            req(&mut f, "series", text());
            req(&mut f, "node", text());
            req(&mut f, "remaining", PayloadType::Int);
            req(&mut f, "total", PayloadType::Int);
        }
        BPM_GOVERNANCE_TOPIC => {
            req(&mut f, "kind", closed(&GOVERNANCE_KINDS));
            // `system`, `user:<id>` or `consumer:<id>`; checked by `check`.
            req(&mut f, "actor", text());
            opt(&mut f, "on_behalf_of", text());
            let mut subject = Fields::new();
            req(&mut subject, "doctype", text());
            req(&mut subject, "id", PayloadType::Id);
            opt(&mut f, "subject", PayloadType::Object(subject));
            // Unix microseconds.
            req(&mut f, "at", PayloadType::Int);
            // Tagged values `{"Text": s}`, `{"Int": n}`, `{"Bool": b}`; checked by `check`.
            req(&mut f, "payload", PayloadType::Json);
            req(&mut f, "masked", PayloadType::Bool);
        }
        BPM_CASE_MIGRATED_TOPIC => {
            req(&mut f, "case", text());
            req(&mut f, "from", PayloadType::Object(version_ref_fields()));
            req(&mut f, "to", PayloadType::Object(version_ref_fields()));
            req(&mut f, "plan_digest", text());
            opt(&mut f, "actor", text());
            req(&mut f, "store_version", PayloadType::Int);
            req(&mut f, "nodes_changed", PayloadType::Int);
            req(&mut f, "variables_defaulted", PayloadType::Int);
            // Unix microseconds.
            req(&mut f, "at", PayloadType::Int);
        }
        CONDITION_RAISED_TOPIC => {
            condition_common(&mut f, &RAISED_TRANSITIONS);
            // A raise carries `severity`; an escalation carries `from` and `to`. Checked by
            // `check`.
            opt(&mut f, "severity", closed(&SEVERITIES));
            opt(&mut f, "from", closed(&SEVERITIES));
            opt(&mut f, "to", closed(&SEVERITIES));
        }
        CONDITION_CLEARED_TOPIC => {
            condition_common(&mut f, &CLEARED_TRANSITIONS);
            req(&mut f, "severity", closed(&SEVERITIES));
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

fn condition_common(f: &mut Fields, transitions: &[&str]) {
    req(f, "transition", closed(transitions));
    // Upper snake case; checked by `check`.
    req(f, "id", text());
    // Opaque text compared exactly.
    req(f, "subject", text());
    // Milliseconds on the caller's own clock, not necessarily since the Unix epoch.
    req(f, "at_millis", PayloadType::Int);
    // A name-to-text map; checked by `check`.
    req(f, "params", PayloadType::Json);
}

fn effect_fields() -> Fields {
    let mut f = Fields::new();
    // `app:step`.
    req(&mut f, "step", text());
    // A storage-style class name.
    req(&mut f, "class", text());
    req(&mut f, "kind", closed(&EFFECT_KINDS));
    req(&mut f, "store_refs", PayloadType::List(Box::new(text())));
    req(&mut f, "count", PayloadType::Int);
    // 64 lower-case hex characters.
    req(&mut f, "digest", text());
    // One of `EFFECT_STATES`, or `{"failed": {"code": ...}}`; checked by `check`.
    req(&mut f, "state", PayloadType::Json);
    f
}

fn version_ref_fields() -> Fields {
    let mut f = Fields::new();
    req(&mut f, "name", text());
    req(&mut f, "version", PayloadType::Int);
    req(&mut f, "hash", text());
    f
}

/// The structural rules of the nine topics' `Json` and `Text` fields, run by
/// [`system::validate`](crate::system::validate) after the strict schema passed.
pub(crate) fn check(topic: &str, data: &Json, errors: &mut Vec<SchemaError>) {
    let Some(obj) = data.as_object() else { return };
    match topic {
        JOB_ENQUEUED_TOPIC => check_job(obj, errors),
        USER_NOTICE_TOPIC => check_notice(obj, errors),
        OPERATION_RECEIPT_TOPIC => check_receipt(obj, errors),
        OPERATION_RECEIPT_EFFECT_TOPIC => {
            if let Some(effect) = obj.get("effect") {
                check_effect(effect, "effect", errors);
            }
        }
        PARTITION_RANGE_LOW_TOPIC => check_range_low(obj, errors),
        BPM_GOVERNANCE_TOPIC => check_governance(obj, errors),
        BPM_CASE_MIGRATED_TOPIC => check_case_migrated(obj, errors),
        CONDITION_RAISED_TOPIC | CONDITION_CLEARED_TOPIC => check_condition(topic, obj, errors),
        _ => {}
    }
}

/// `[a-z][a-z0-9_]*`, at most 63 bytes: a storage-style name.
fn is_name(s: &str) -> bool {
    valid_property_name(s)
}

/// `<app>:<name>` with both parts storage-style names: a job or a step.
fn is_qualified_name(s: &str) -> bool {
    s.split_once(':')
        .is_some_and(|(a, n)| is_name(a) && is_name(n))
}

/// Whether `value` holds a JSON number that is not an integer, at any depth.
fn has_float(value: &Json) -> bool {
    match value {
        Json::Number(n) => n.is_f64(),
        Json::Array(items) => items.iter().any(has_float),
        Json::Object(map) => map.values().any(has_float),
        _ => false,
    }
}

fn int(obj: &Map<String, Json>, field: &str) -> Option<i64> {
    obj.get(field).and_then(Json::as_i64)
}

fn check_job(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    match obj.get("job").and_then(Json::as_str) {
        Some(job) if is_qualified_name(job) => {}
        _ => errors.push(err("job", "a job is <app>:<job>, both storage-style names")),
    }
    if let Some(args) = obj.get("args")
        && has_float(args)
    {
        errors.push(err(
            "args",
            "no floating-point number; a decimal is a string (ADR-055)",
        ));
    }
    if let Some(Json::String(key)) = obj.get("idempotency_key")
        && key.is_empty()
    {
        errors.push(err("idempotency_key", "an idempotency key is not empty"));
    }
    if let Some(Json::String(at)) = obj.get("not_before")
        && time::OffsetDateTime::parse(at, &Rfc3339).is_err()
    {
        errors.push(err("not_before", "an RFC 3339 time"));
    }
}

/// A notice scalar: null, a boolean, an integer or a string within [`NOTICE_MAX_TEXT_BYTES`].
fn notice_scalar(value: &Json) -> bool {
    match value {
        Json::Null | Json::Bool(_) => true,
        Json::Number(n) => !n.is_f64(),
        Json::String(s) => s.len() <= NOTICE_MAX_TEXT_BYTES,
        Json::Array(_) | Json::Object(_) => false,
    }
}

fn check_notice(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    let Some(body) = obj.get("payload") else {
        return;
    };
    let Some(fields) = body.as_object() else {
        errors.push(err("payload", "an object"));
        return;
    };
    let size = serde_json::to_vec(body).map_or(usize::MAX, |v| v.len());
    if size > NOTICE_MAX_BYTES {
        errors.push(err(
            "payload",
            format!("at most {NOTICE_MAX_BYTES} bytes of compact JSON"),
        ));
    }
    for (name, value) in fields {
        let path = payload_path(name);
        if name.len() > NOTICE_MAX_TEXT_BYTES {
            errors.push(err(&path, "a key of at most 256 bytes"));
        }
        let ok = match value {
            Json::Array(items) => items.iter().all(notice_scalar),
            other => notice_scalar(other),
        };
        if !ok {
            errors.push(err(
                &path,
                "a scalar or an array of scalars (no object, no nested array, no float, no string over 256 bytes)",
            ));
        }
    }
}

/// The failure code shape the engine keeps: a lower-case letter, then lower-case letters, digits,
/// dots and underscores, at most [`MAX_FAILURE_CODE_BYTES`] bytes.
fn is_failure_code(code: &str) -> bool {
    code.len() <= MAX_FAILURE_CODE_BYTES
        && code.starts_with(|c: char| c.is_ascii_lowercase())
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_')
}

fn check_state(state: &Json, path: &str, errors: &mut Vec<SchemaError>) {
    let ok = match state {
        Json::String(s) => EFFECT_STATES.contains(&s.as_str()),
        Json::Object(map) if map.len() == 1 => map
            .get("failed")
            .and_then(Json::as_object)
            .is_some_and(|failed| {
                failed.len() == 1
                    && failed
                        .get("code")
                        .and_then(Json::as_str)
                        .is_some_and(is_failure_code)
            }),
        _ => false,
    };
    if !ok {
        errors.push(err(
            path,
            "a state is applied, held, released, delivered or {\"failed\": {\"code\": <stable code>}}",
        ));
    }
}

fn check_effect(effect: &Json, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(obj) = effect.as_object() else {
        return;
    };
    let at = |field: &str| format!("{path}.{field}");
    match obj.get("step").and_then(Json::as_str) {
        Some(step) if is_qualified_name(step) => {}
        _ => errors.push(err(&at("step"), "a step is <app>:<step>")),
    }
    match obj.get("class").and_then(Json::as_str) {
        Some(class) if is_name(class) => {}
        _ => errors.push(err(&at("class"), "a storage-style class name")),
    }
    match obj.get("digest").and_then(Json::as_str) {
        Some(digest) if is_hex_digest(digest) => {}
        _ => errors.push(err(&at("digest"), "64 lower-case hex characters")),
    }
    match int(obj, "count") {
        Some(n) if (0..=i64::from(u32::MAX)).contains(&n) => {}
        _ => errors.push(err(&at("count"), "a count from 0 to 4294967295")),
    }
    if let Some(Json::Array(refs)) = obj.get("store_refs") {
        for (i, r) in refs.iter().enumerate() {
            let ok = r.as_str().is_some_and(|s| {
                !s.is_empty() && s.len() <= MAX_REF_BYTES && !s.chars().any(char::is_control)
            });
            if !ok {
                errors.push(err(
                    &format!("{}[{i}]", at("store_refs")),
                    format!("1 to {MAX_REF_BYTES} bytes with no control character"),
                ));
            }
        }
    }
    if let Some(state) = obj.get("state") {
        check_state(state, &at("state"), errors);
    }
}

fn check_receipt(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    match obj.get("doctype").and_then(Json::as_str) {
        Some(doctype) if is_name(doctype) => {}
        _ => errors.push(err("doctype", "a DocType name")),
    }
    if int(obj, "version").is_some_and(|v| v < 0) {
        errors.push(err("version", "a version is not negative"));
    }
    let effects = obj
        .get("effects")
        .and_then(Json::as_array)
        .map_or(&[][..], Vec::as_slice);
    // A receipt event exists only when at least one effect was reported.
    if effects.is_empty() || effects.len() > MAX_EFFECTS_PER_RECEIPT {
        errors.push(err(
            "effects",
            format!("1 to {MAX_EFFECTS_PER_RECEIPT} effects"),
        ));
    }
    let mut refs = 0usize;
    for (i, effect) in effects.iter().enumerate() {
        check_effect(effect, &format!("effects[{i}]"), errors);
        refs += effect
            .get("store_refs")
            .and_then(Json::as_array)
            .map_or(0, Vec::len);
    }
    if refs > MAX_REFS_PER_RECEIPT {
        errors.push(err(
            "effects",
            format!("at most {MAX_REFS_PER_RECEIPT} references in all"),
        ));
    }
    if let Some(Json::Array(list)) = obj.get("undeclared") {
        let mut seen = BTreeSet::new();
        for (i, v) in list.iter().enumerate() {
            let ok = v.as_u64().is_some_and(|n| {
                usize::try_from(n).is_ok_and(|n| n < effects.len()) && seen.insert(n)
            });
            if !ok {
                errors.push(err(
                    &format!("undeclared[{i}]"),
                    "a distinct index into effects",
                ));
            }
        }
    }
}

/// A node id as the sync protocol writes it: a lower-case host name, optionally `#` and a
/// storage name.
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
    host && partition.is_none_or(is_name)
}

fn check_range_low(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    match obj.get("series").and_then(Json::as_str) {
        Some(s)
            if !s.is_empty() && s.len() <= MAX_SERIES_BYTES && !s.chars().any(char::is_control) => {
        }
        _ => errors.push(err(
            "series",
            format!("1 to {MAX_SERIES_BYTES} bytes of printable text"),
        )),
    }
    match obj.get("node").and_then(Json::as_str) {
        Some(n) if is_node_id(n) => {}
        _ => errors.push(err("node", "a node id: <host>[#<partition>]")),
    }
    let (remaining, total) = (int(obj, "remaining"), int(obj, "total"));
    match (remaining, total) {
        (Some(r), Some(t)) if r >= 0 && r <= t => {}
        _ => errors.push(err(
            "remaining",
            "remaining is not negative and not more than total",
        )),
    }
}

/// `system`, `user:<id>` or `consumer:<id>`.
fn is_principal(text: &str) -> bool {
    text == crate::payload::SYSTEM_PRINCIPAL
        || text
            .parse::<PrincipalRef>()
            .is_ok_and(|p| matches!(p.kind(), PrincipalKind::User | PrincipalKind::Consumer))
}

/// One tagged value of the audit chain's payload: `{"Text": s}`, `{"Int": n}` or `{"Bool": b}`.
fn tagged(v: &Json) -> Option<(&str, &Json)> {
    let map = v.as_object().filter(|m| m.len() == 1)?;
    let (tag, inner) = map.iter().next()?;
    let ok = match tag.as_str() {
        "Text" => inner.is_string(),
        "Int" => inner.is_i64(),
        "Bool" => inner.is_boolean(),
        _ => false,
    };
    ok.then_some((tag.as_str(), inner))
}

fn check_governance(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    let kind = obj.get("kind").and_then(Json::as_str).unwrap_or_default();
    if !is_principal_text(obj.get("actor")) {
        errors.push(err("actor", "system, user:<id> or consumer:<id>"));
    }
    match obj.get("on_behalf_of") {
        None | Some(Json::Null) => {}
        Some(Json::String(s)) if s.starts_with("user:") && is_principal(s) => {}
        Some(_) => errors.push(err("on_behalf_of", "a user:<id> reference")),
    }
    if obj.get("masked").and_then(Json::as_bool) != Some(true) {
        errors.push(err(
            "masked",
            "always true: an entry carries no document value",
        ));
    }
    let Some(payload) = obj.get("payload").and_then(Json::as_object) else {
        errors.push(err("payload", "an object of tagged values"));
        return;
    };
    for (key, value) in payload {
        let path = payload_path(key);
        if !GOVERNANCE_PAYLOAD_KEYS.contains(&key.as_str()) {
            errors.push(err(&path, "not a governance entry key"));
        } else if !tagged(value).is_some_and(|(tag, _)| tag == "Text") {
            errors.push(err(&path, "a {\"Text\": s} value"));
        }
    }
    let text_of = |key: &str| {
        payload
            .get(key)
            .and_then(|v| tagged(v))
            .and_then(|(_, inner)| inner.as_str())
    };
    if text_of("company").is_none_or(str::is_empty) {
        errors.push(err(&payload_path("company"), "required"));
    }
    if !text_of("mode").is_some_and(|m| GOVERNANCE_MODES.contains(&m)) {
        errors.push(err(&payload_path("mode"), "free, governed or process_only"));
    }
    if text_of("policy") != Some(GOVERNANCE_POLICY) {
        errors.push(err(&payload_path("policy"), "the governance policy key"));
    }
    // What each kind carries and omits, as the producer's constructors build them.
    let subject = present(obj, "subject");
    let (reason, detail) = (
        payload.contains_key("reason"),
        payload.contains_key("detail"),
    );
    let shape = match kind {
        k if k == GOVERNANCE_KINDS[0] => {
            subject && reason && !detail && text_of("mode") == Some("governed")
        }
        k if k == GOVERNANCE_KINDS[1] => subject && !reason && detail,
        k if k == GOVERNANCE_KINDS[2] => {
            !subject && !reason && detail && !payload.contains_key("command_id")
        }
        _ => true,
    };
    if !shape {
        errors.push(err(
            "payload",
            "an exception has a subject and a reason; a refusal a subject and a detail; a policy change a detail and no subject, reason or command",
        ));
    }
    if payload.contains_key("doctype") != (subject || kind == GOVERNANCE_KINDS[2]) {
        errors.push(err(
            &payload_path("doctype"),
            "present exactly when the entry is about a record type",
        ));
    }
    if let Some(Json::Object(s)) = obj.get("subject")
        && payload.contains_key("doctype")
        && text_of("doctype") != s.get("doctype").and_then(Json::as_str)
    {
        errors.push(err(
            &path_under("subject", "doctype"),
            "the same record type as payload.doctype",
        ));
    }
}

fn is_principal_text(v: Option<&Json>) -> bool {
    v.and_then(Json::as_str).is_some_and(is_principal)
}

fn check_case_migrated(obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    // A process run id: `[a-z][a-z0-9_]{0,63}`.
    match obj.get("case").and_then(Json::as_str) {
        Some(case) if is_name(case) => {}
        _ => errors.push(err("case", "a case id: [a-z][a-z0-9_]*, at most 64 bytes")),
    }
    for side in ["from", "to"] {
        let Some(pin) = obj.get(side).and_then(Json::as_object) else {
            continue;
        };
        match pin.get("name").and_then(Json::as_str) {
            Some(name) if !name.is_empty() => {}
            _ => errors.push(err(&format!("{side}.name"), "a process name")),
        }
        match pin.get("version").and_then(Json::as_i64) {
            Some(v) if (0..=i64::from(u32::MAX)).contains(&v) => {}
            _ => errors.push(err(
                &format!("{side}.version"),
                "a version from 0 to 4294967295",
            )),
        }
        match pin.get("hash").and_then(Json::as_str) {
            Some(h) if is_hex_digest(h) => {}
            _ => errors.push(err(&format!("{side}.hash"), "64 lower-case hex characters")),
        }
    }
    match obj.get("plan_digest").and_then(Json::as_str) {
        Some(d) if is_hex_digest(d) => {}
        _ => errors.push(err("plan_digest", "64 lower-case hex characters")),
    }
    if let Some(Json::String(actor)) = obj.get("actor")
        && actor.is_empty()
    {
        errors.push(err("actor", "not empty when present"));
    }
    for field in ["store_version", "nodes_changed", "variables_defaulted"] {
        if int(obj, field).is_some_and(|n| n < 0) {
            errors.push(err(field, "not negative"));
        }
    }
}

/// `[A-Z][A-Z0-9]*(_[A-Z0-9]+)*`, at most [`MAX_CONDITION_ID_BYTES`] bytes.
fn is_condition_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_CONDITION_ID_BYTES
        && b[0].is_ascii_uppercase()
        && !s.ends_with('_')
        && !s.contains("__")
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

fn severity_rank(v: Option<&Json>) -> Option<usize> {
    let s = v.and_then(Json::as_str)?;
    SEVERITIES.iter().position(|x| *x == s)
}

fn check_condition(topic: &str, obj: &Map<String, Json>, errors: &mut Vec<SchemaError>) {
    match obj.get("id").and_then(Json::as_str) {
        Some(id) if is_condition_id(id) => {}
        _ => errors.push(err("id", "an upper snake case condition id")),
    }
    if obj
        .get("at_millis")
        .and_then(Json::as_i64)
        .is_some_and(|n| n < 0)
    {
        errors.push(err("at_millis", "not negative"));
    }
    match obj.get("params").and_then(Json::as_object) {
        Some(params) => {
            if params.len() > MAX_CONDITION_PARAMS {
                errors.push(err(
                    "params",
                    format!("at most {MAX_CONDITION_PARAMS} parameters"),
                ));
            }
            for (name, value) in params {
                if !value.is_string() {
                    errors.push(err(&format!("params.{name}"), "a text value"));
                }
            }
        }
        None => errors.push(err("params", "an object of text values")),
    }
    if topic != CONDITION_RAISED_TOPIC {
        return;
    }
    let (severity, from, to) = (
        present(obj, "severity"),
        present(obj, "from"),
        present(obj, "to"),
    );
    match obj.get("transition").and_then(Json::as_str) {
        Some("raised") if severity && !from && !to => {}
        Some("escalated") if !severity && from && to => {
            if severity_rank(obj.get("from")) >= severity_rank(obj.get("to")) {
                errors.push(err(
                    "to",
                    "an escalation rises: to is more severe than from",
                ));
            }
        }
        _ => errors.push(err(
            "transition",
            "a raise carries severity only; an escalation carries from and to only",
        )),
    }
}

/// The path of a payload key in an error (`payload.<key>`), built so the key is not a dotted literal.
fn payload_path(key: &str) -> String {
    path_under("payload", key)
}

/// A dotted error path, `<parent>.<key>`, built so neither part is a dotted literal.
fn path_under(parent: &str, key: &str) -> String {
    format!("{parent}.{key}")
}
