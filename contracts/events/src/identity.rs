//! The platform's identity and access topics (ADR-070, work package WP-IAM-01).
//!
//! Every change to who may do what, and every sign-in, session and key event, is an
//! `identity_*` event written in the same transaction as the change (ADR-11), so the audit
//! trail, the journal (ADR-061) and the access reviews (WP-IAM-08) read one stream. The prefix
//! `identity_` is reserved for the platform, like `doc_` and `sys_`.
//!
//! # No secret in an event
//!
//! An identity event never carries a credential. A password, a token, an API-key secret, a TOTP
//! secret, a recovery code, a private key or a key's material is not a field of any schema here,
//! and [`validate`] rejects a payload that holds any such key **at any depth**, including inside
//! the free-form `Json` fields (`before`, `after`, `scope`). The deny list is
//! [`FORBIDDEN_KEY_PARTS`]: a key containing one of its entries, ignoring case, is refused.
//!
//! # Who is the actor
//!
//! [`Actor::User`](crate::Actor) carries `via` when one identity acts as another (impersonation,
//! ADR-070 section 9): `id` is the identity whose rights applied, `via` is the real person.
//! Events caused during impersonation keep both, and `identity_impersonation_started` and
//! `identity_impersonation_ended` bracket the session.
//!
//! The subject of each event is the record it is about (the user, the assignment, the role); the
//! payloads below add what the audit trail and the reviews need. Types follow the native API
//! value encoding (ADR-055). Enums marked *open* may gain values within a major version; a
//! consumer must handle values it does not know.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use crate::names::Topic;
use crate::schema::{EventSchema, FieldSchema, PayloadType, SchemaError};

/// Key fragments that no identity payload may contain at any depth (case-insensitive).
///
/// `secret` also catches `api_key_secret` and `totp_secret`; `token` catches `refresh_token` and
/// `access_token` (a token **id** is `session` or `key`, never `token`); `credential`,
/// `private`, `recovery_code`, `passphrase`, `password`, `totp`, `pin_code` and `key_material` cover the
/// rest. The list errs on the side of refusing; a harmless field that trips it is renamed.
pub const FORBIDDEN_KEY_PARTS: [&str; 10] = [
    "password",
    "passphrase",
    "secret",
    "token",
    "credential",
    "private",
    "recovery_code",
    "key_material",
    "totp",
    "pin_code",
];

/// Every identity topic and its short name.
pub const TOPICS: [&str; 33] = [
    "identity_user_created",
    "identity_user_enabled",
    "identity_user_disabled",
    "identity_user_anonymised",
    "identity_sign_in_succeeded",
    "identity_sign_in_failed",
    "identity_account_locked",
    "identity_account_unlocked",
    "identity_recovery_requested",
    "identity_recovery_completed",
    "identity_session_ended",
    "identity_api_key_created",
    "identity_api_key_revoked",
    "identity_role_changed",
    "identity_duty_changed",
    "identity_assignment_created",
    "identity_assignment_changed",
    "identity_assignment_ended",
    "identity_sso_policy_changed",
    "identity_sod_override",
    "identity_emergency_started",
    "identity_emergency_ended",
    "identity_emergency_reviewed",
    "identity_impersonation_started",
    "identity_impersonation_ended",
    "identity_field_accessed",
    // Added in 0.3.0 (amendment A2).
    "identity_session_started",
    "identity_password_changed",
    "identity_second_factor_enrolled",
    "identity_second_factor_removed",
    "identity_recovery_codes_regenerated",
    "identity_sign_in_failed_burst",
    "identity_access_review_completed",
];

/// Topics for the node and snapshot events of ADR-070 rule 12.13.
pub const NODE_TOPICS: [&str; 2] = ["identity_node_changed", "identity_snapshot_published"];

type Fields = BTreeMap<String, FieldSchema>;

fn text() -> PayloadType {
    PayloadType::Text
}

fn id() -> PayloadType {
    PayloadType::Id
}

fn open(values: &[&str]) -> PayloadType {
    PayloadType::Enum {
        values: values.iter().map(|v| (*v).to_owned()).collect(),
        open: true,
    }
}

fn req(fields: &mut Fields, name: &str, ty: PayloadType) {
    fields.insert(name.to_owned(), FieldSchema::required(ty));
}

fn opt(fields: &mut Fields, name: &str, ty: PayloadType) {
    fields.insert(name.to_owned(), FieldSchema::optional(ty));
}

fn schema(topic: &str, fields: Fields) -> EventSchema {
    let topic = match Topic::any(topic) {
        Ok(t) => t,
        Err(e) => unreachable!("identity topic {topic} is invalid: {e}"),
    };
    EventSchema {
        topic,
        major: 1,
        revision: 0,
        fields,
        retired: BTreeSet::new(),
    }
}

const USER_SOURCES: [&str; 6] = ["admin", "invitation", "scim", "sso_jit", "import", "rule"];
const SIGN_IN_METHODS: [&str; 8] = [
    "password",
    "totp",
    "recovery_code",
    "oidc",
    "saml",
    "api_key",
    "token",
    "magic_link",
];
const ASSIGNMENT_SOURCES: [&str; 4] = ["manual", "rule", "idp", "emergency"];

/// The v1 payload schema of one identity topic, or `None` for a name that is not one.
pub fn schema_of(topic: &str) -> Option<EventSchema> {
    let mut f = Fields::new();
    match topic {
        "identity_user_created" => {
            req(&mut f, "user", id());
            req(
                &mut f,
                "user_type",
                open(&["internal", "portal", "service", "agent"]),
            );
            req(&mut f, "source", open(&USER_SOURCES));
        }
        "identity_user_enabled" => {
            req(&mut f, "user", id());
            opt(&mut f, "reason", text());
        }
        "identity_user_disabled" => {
            req(&mut f, "user", id());
            req(
                &mut f,
                "source",
                open(&["admin", "scim", "hr", "rule", "policy"]),
            );
            opt(&mut f, "reason", text());
            // The leaver actions carried out: `end_sessions`, `revoke_refresh_tokens`,
            // `disable_api_keys`, `cancel_invitations`, `reassign_open_tasks`.
            req(&mut f, "actions", PayloadType::List(Box::new(text())));
        }
        "identity_user_anonymised" => {
            req(&mut f, "user", id());
        }
        "identity_sign_in_succeeded" => {
            req(&mut f, "user", id());
            req(&mut f, "method", open(&SIGN_IN_METHODS));
            opt(&mut f, "session", id());
            req(&mut f, "step_up", PayloadType::Bool);
            opt(&mut f, "source_address", text());
            opt(&mut f, "device_label", text());
        }
        "identity_sign_in_failed" => {
            // Absent when the name matched no user, so the log never confirms a name.
            opt(&mut f, "user", id());
            req(&mut f, "method", open(&SIGN_IN_METHODS));
            req(
                &mut f,
                "reason",
                open(&[
                    "bad_credentials",
                    "unknown_user",
                    "disabled",
                    "locked",
                    "sso_required",
                    "policy",
                ]),
            );
            opt(&mut f, "source_address", text());
        }
        "identity_account_locked" => {
            req(&mut f, "user", id());
            req(
                &mut f,
                "reason",
                open(&["failed_attempts", "admin", "policy"]),
            );
            opt(&mut f, "until", PayloadType::DateTime);
        }
        "identity_account_unlocked" => {
            req(&mut f, "user", id());
            // Absent when the lock expired by itself.
            opt(&mut f, "by", id());
        }
        "identity_recovery_requested" | "identity_recovery_completed" => {
            req(&mut f, "user", id());
            req(&mut f, "kind", open(&["password", "second_factor"]));
            if topic == "identity_recovery_requested" {
                req(&mut f, "initiated_by", id());
                // Steps required: `step_up`, `second_approver`, `notify_all_addresses`.
                req(&mut f, "steps", PayloadType::List(Box::new(text())));
            }
        }
        "identity_session_ended" => {
            req(&mut f, "session", id());
            req(&mut f, "user", id());
            req(
                &mut f,
                "reason",
                open(&[
                    "sign_out",
                    "idle_timeout",
                    "absolute_timeout",
                    "revoked",
                    "user_disabled",
                    "policy",
                ]),
            );
        }
        "identity_api_key_created" => {
            req(&mut f, "key", id());
            req(&mut f, "owner", id());
            // A readable summary of the scope; the scope itself is the key's record.
            req(&mut f, "scope", PayloadType::List(Box::new(text())));
            opt(&mut f, "expires_at", PayloadType::DateTime);
        }
        "identity_api_key_revoked" => {
            req(&mut f, "key", id());
            req(&mut f, "owner", id());
            req(
                &mut f,
                "reason",
                open(&["revoked", "owner_disabled", "expired", "rotated"]),
            );
        }
        "identity_role_changed" | "identity_duty_changed" => {
            req(&mut f, "name", text());
            req(
                &mut f,
                "change",
                open(&["created", "updated", "deleted", "copied", "upgraded"]),
            );
            req(&mut f, "origin", open(&["reference", "custom"]));
            opt(&mut f, "version", text());
            opt(&mut f, "copied_from", text());
            opt(&mut f, "before", PayloadType::Json);
            opt(&mut f, "after", PayloadType::Json);
        }
        "identity_assignment_created" => {
            req(&mut f, "assignment", id());
            req(&mut f, "user", id());
            req(&mut f, "role", text());
            req(&mut f, "source", open(&ASSIGNMENT_SOURCES));
            req(&mut f, "scope", PayloadType::Json);
            opt(&mut f, "valid_from", PayloadType::DateTime);
            opt(&mut f, "valid_to", PayloadType::DateTime);
            opt(&mut f, "reason", text());
        }
        "identity_assignment_changed" => {
            req(&mut f, "assignment", id());
            req(&mut f, "user", id());
            req(&mut f, "role", text());
            req(&mut f, "before", PayloadType::Json);
            req(&mut f, "after", PayloadType::Json);
            opt(&mut f, "reason", text());
        }
        "identity_assignment_ended" => {
            req(&mut f, "assignment", id());
            req(&mut f, "user", id());
            req(&mut f, "role", text());
            req(&mut f, "source", open(&ASSIGNMENT_SOURCES));
            req(
                &mut f,
                "cause",
                open(&[
                    "removed",
                    "expired",
                    "rule_ended",
                    "idp_removed",
                    "user_disabled",
                ]),
            );
        }
        "identity_sso_policy_changed" => {
            req(
                &mut f,
                "mode",
                open(&["local", "sso_optional", "sso_required"]),
            );
            opt(&mut f, "provider", text());
            req(&mut f, "break_glass_count", PayloadType::Int);
            opt(&mut f, "before", PayloadType::Json);
        }
        "identity_sod_override" => {
            req(&mut f, "assignment", id());
            req(&mut f, "user", id());
            req(&mut f, "rule", text());
            req(&mut f, "reason", text());
            req(&mut f, "mitigating_control", text());
            req(&mut f, "approved_by", id());
        }
        "identity_emergency_started" => {
            req(&mut f, "grant", id());
            req(&mut f, "user", id());
            req(&mut f, "role", text());
            req(&mut f, "approved_by", id());
            req(&mut f, "expires_at", PayloadType::DateTime);
            req(&mut f, "reason", text());
        }
        "identity_emergency_ended" => {
            req(&mut f, "grant", id());
            req(&mut f, "user", id());
            req(&mut f, "ended_by", open(&["user", "expiry", "revoked"]));
        }
        "identity_emergency_reviewed" => {
            req(&mut f, "grant", id());
            req(&mut f, "controller", id());
            req(&mut f, "outcome", open(&["confirmed", "flagged"]));
            opt(&mut f, "notes", text());
        }
        "identity_impersonation_started" => {
            req(&mut f, "impersonator", id());
            req(&mut f, "target", id());
            req(&mut f, "grant", id());
            req(&mut f, "expires_at", PayloadType::DateTime);
        }
        "identity_impersonation_ended" => {
            req(&mut f, "impersonator", id());
            req(&mut f, "target", id());
            req(
                &mut f,
                "ended_by",
                open(&["impersonator", "target", "expiry", "revoked"]),
            );
        }
        "identity_field_accessed" => {
            // The field access trace (ADR-070 section 12 rule 9): never the value.
            req(&mut f, "user", id());
            req(&mut f, "doctype", text());
            opt(&mut f, "document", id());
            req(&mut f, "group", text());
            opt(&mut f, "field", text());
            req(&mut f, "right", open(&["read", "reveal", "reveal_bulk"]));
            req(&mut f, "authorised", PayloadType::Bool);
            req(
                &mut f,
                "channel",
                open(&["api", "ui", "report", "export", "realtime", "sync", "job"]),
            );
        }
        "identity_node_changed" => {
            req(&mut f, "node", text());
            req(
                &mut f,
                "change",
                open(&[
                    "enrolled",
                    "suspended",
                    "resumed",
                    "decommissioned",
                    "certificate_renewed",
                ]),
            );
            opt(&mut f, "reason", text());
            // Whether the data keys of the groups the node held were rotated (ADR-070 12.13).
            opt(&mut f, "keys_rotated", PayloadType::Bool);
        }
        "identity_snapshot_published" => {
            opt(&mut f, "node", text());
            req(&mut f, "epoch", PayloadType::Int);
            req(&mut f, "hash", text());
            opt(&mut f, "valid_until", PayloadType::DateTime);
        }
        "identity_session_started" => {
            req(&mut f, "session", id());
            req(&mut f, "user", id());
            req(&mut f, "method", open(&SIGN_IN_METHODS));
            req(&mut f, "expires_at", PayloadType::DateTime);
            opt(&mut f, "device_label", text());
        }
        "identity_password_changed" => {
            req(&mut f, "user", id());
            req(&mut f, "by", open(&["user", "admin", "recovery", "policy"]));
            // How many other sessions the change ended.
            req(&mut f, "sessions_ended", PayloadType::Int);
        }
        "identity_second_factor_enrolled" | "identity_second_factor_removed" => {
            req(&mut f, "user", id());
            req(
                &mut f,
                "factor",
                open(&["totp", "webauthn", "sms", "email"]),
            );
            // The id of the factor record, never its secret.
            req(&mut f, "factor_id", id());
            if topic == "identity_second_factor_removed" {
                req(&mut f, "by", open(&["user", "admin", "recovery"]));
            }
        }
        "identity_recovery_codes_regenerated" => {
            req(&mut f, "user", id());
            // How many codes the new set holds; the codes are shown once and never stored here.
            req(&mut f, "count", PayloadType::Int);
            req(&mut f, "by", open(&["user", "admin"]));
        }
        "identity_sign_in_failed_burst" => {
            // The rate summary an emitter may write instead of one `identity_sign_in_failed`
            // per attempt once a window passes `threshold` failures. Per-attempt events resume
            // when the window closes below the threshold.
            opt(&mut f, "user", id());
            req(&mut f, "window_start", PayloadType::DateTime);
            req(&mut f, "window_end", PayloadType::DateTime);
            req(&mut f, "attempts", PayloadType::Int);
            // Distinct source addresses seen, as a count; the addresses stay in the log.
            req(&mut f, "sources", PayloadType::Int);
            // The per-window failure count above which the emitter summarises.
            req(&mut f, "threshold", PayloadType::Int);
            req(&mut f, "method", open(&SIGN_IN_METHODS));
        }
        "identity_access_review_completed" => {
            req(&mut f, "review", id());
            req(&mut f, "reviewer", id());
            req(&mut f, "items", PayloadType::Int);
            req(&mut f, "confirmed", PayloadType::Int);
            req(&mut f, "revoked", PayloadType::Int);
            opt(&mut f, "note", text());
        }
        _ => return None,
    }
    Some(schema(topic, f))
}

/// The longest string any identity payload may hold, in characters, at any depth.
///
/// Identity events are audit records read for years: a bounded string cannot smuggle a blob, a
/// pasted secret or a stack trace into them. [`validate`] enforces it.
pub const MAX_STRING_CHARS: usize = 256;

/// Delivery of every identity event (ADR-056, amendment A2): `at-least-once`, so a consumer
/// deduplicates on the event id, and kept as audit records for the audit retention period, not
/// the shorter outbox retention. There is no best-effort identity event.
pub const AT_LEAST_ONCE: bool = true;

/// Whether the event is an audit record (kept per the audit retention). True for every topic.
#[deprecated(since = "0.7.1", note = "use `registry::is_audit`; removed in 0.8.0")]
pub fn is_audit(topic: &str) -> bool {
    schema_of(topic).is_some() && crate::registry::is_audit(topic)
}

/// `Text` and `Json` fields that are free-form by design. The contract test fails on any other
/// `Text` or `Json` field in an identity schema, so a new field of those types is a deliberate,
/// reviewed addition here, and a secret cannot arrive as an innocent-looking string.
pub const FREE_FORM_FIELDS: &[&str] = &[
    "name",
    "version",
    "copied_from",
    "before",
    "after",
    "scope",
    "role",
    "reason",
    "actions",
    "provider",
    "rule",
    "mitigating_control",
    "notes",
    "note",
    "doctype",
    "group",
    "field",
    "node",
    "hash",
    "steps",
    "source_address",
    "device_label",
];

/// The v1 schemas of every identity topic, including the node topics.
pub fn schemas() -> Vec<EventSchema> {
    TOPICS
        .iter()
        .chain(NODE_TOPICS.iter())
        .filter_map(|t| schema_of(t))
        .collect()
}

/// Validates an identity event's `data`: strictly against the topic's schema, then scans every
/// key at every depth (including inside `Json` fields) for [`FORBIDDEN_KEY_PARTS`].
///
/// A topic that is not an identity topic is reported as an error on the empty path.
pub fn validate(topic: &str, data: &Json) -> Result<(), Vec<SchemaError>> {
    let Some(schema) = schema_of(topic) else {
        return Err(vec![SchemaError {
            path: String::new(),
            message: format!("{topic} is not an identity topic"),
        }]);
    };
    let mut errors = schema.validate(data).err().unwrap_or_default();
    scan_keys(data, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn scan_keys(v: &Json, path: &str, errors: &mut Vec<SchemaError>) {
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
                    errors.push(SchemaError {
                        path: here.clone(),
                        message: "an identity event never carries a credential or secret".into(),
                    });
                }
                scan_keys(child, &here, errors);
            }
        }
        Json::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                scan_keys(child, &format!("{path}[{i}]"), errors);
            }
        }
        Json::String(text) if text.chars().count() > MAX_STRING_CHARS => {
            errors.push(SchemaError {
                path: path.to_owned(),
                message: format!(
                    "an identity event string is at most {MAX_STRING_CHARS} characters"
                ),
            });
        }
        _ => {}
    }
}
