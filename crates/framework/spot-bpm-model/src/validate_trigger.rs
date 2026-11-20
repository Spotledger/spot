//! The optional trigger fields (WP-BPM-01.T5): start roles, input schema,
//! document condition, schedule zone and missed-run policy.
//!
//! Each fault is one code; the caller anchors it on the trigger node. A value
//! is never echoed (WP-BPM-01.T3 rule): only the node id and the fault kind
//! leave this module.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::json::find_float;
use crate::model::{MissedPolicy, TriggerConfig, TriggerType};
use crate::validate::codes;

/// Most start roles one trigger may name.
const MAX_ROLES: usize = 32;
/// Longest role name, in bytes.
const MAX_ROLE_BYTES: usize = 128;
/// Largest input schema text, in bytes.
const MAX_SCHEMA_BYTES: usize = 16 * 1024;
/// Deepest nesting an input schema may have.
const MAX_SCHEMA_DEPTH: usize = 32;
/// Longest condition text, in bytes.
const MAX_CONDITION_BYTES: usize = 2_000;
/// Longest zone name, in bytes.
const MAX_ZONE_BYTES: usize = 64;
/// Largest `catch_up_max`.
const MAX_CATCH_UP: u32 = 1_000;

/// Every fault in a trigger's optional fields, one code per fault.
pub(crate) fn check_fields(config: &TriggerConfig) -> Vec<&'static str> {
    let mut out = Vec::new();
    let start = matches!(config.trigger, TriggerType::Manual | TriggerType::Api);
    let schedule = matches!(config.trigger, TriggerType::Schedule);
    let doc_event = matches!(config.trigger, TriggerType::DocumentEvent);

    if !config.roles.is_empty() {
        if !start {
            out.push(codes::TRIGGER_FIELD_MISPLACED);
        } else if !roles_ok(&config.roles) {
            out.push(codes::TRIGGER_FIELD_INVALID);
        }
    }
    if let Some(schema) = &config.input_schema {
        if !start {
            out.push(codes::TRIGGER_FIELD_MISPLACED);
        } else if !schema_ok(schema) {
            out.push(codes::TRIGGER_FIELD_INVALID);
        }
    }
    if let Some(condition) = &config.condition {
        if !doc_event {
            out.push(codes::TRIGGER_FIELD_MISPLACED);
        } else if !condition_ok(condition) {
            out.push(codes::TRIGGER_FIELD_INVALID);
        }
    }
    if let Some(zone) = &config.zone {
        if !schedule {
            out.push(codes::TRIGGER_FIELD_MISPLACED);
        } else if !zone_ok(zone) {
            out.push(codes::TRIGGER_FIELD_INVALID);
        }
    }
    if config.missed.is_some() && !schedule {
        out.push(codes::TRIGGER_FIELD_MISPLACED);
    }
    if config.catch_up_max.is_some() && !schedule {
        out.push(codes::TRIGGER_FIELD_MISPLACED);
    }
    if schedule {
        match (config.missed, config.catch_up_max) {
            (Some(MissedPolicy::CatchUp), Some(max)) if !(1..=MAX_CATCH_UP).contains(&max) => {
                out.push(codes::TRIGGER_FIELD_INVALID);
            }
            (Some(MissedPolicy::CatchUp), None) => out.push(codes::TRIGGER_FIELD_INVALID),
            (Some(MissedPolicy::CatchUp), Some(_)) | (None, None) => {}
            (_, Some(_)) => out.push(codes::TRIGGER_FIELD_INVALID),
            (Some(_), None) => {}
        }
    }
    out
}

/// At most 32 roles of 1..=128 bytes, no whitespace or control, no duplicates.
fn roles_ok(roles: &[String]) -> bool {
    if roles.len() > MAX_ROLES {
        return false;
    }
    let mut seen = BTreeSet::new();
    roles.iter().all(|r| {
        !r.is_empty()
            && r.len() <= MAX_ROLE_BYTES
            && !r.chars().any(|c| c.is_control() || c.is_whitespace())
            && seen.insert(r.as_str())
    })
}

/// A JSON object of at most 16 KiB, no float, nesting at most 32.
fn schema_ok(text: &str) -> bool {
    if text.len() > MAX_SCHEMA_BYTES {
        return false;
    }
    match serde_json::from_str::<Value>(text) {
        Ok(value @ Value::Object(_)) => {
            find_float(&value, "").is_none() && depth(&value) <= MAX_SCHEMA_DEPTH
        }
        _ => false,
    }
}

/// Nesting depth of a JSON value: a scalar is 0, an object or array is one
/// more than its deepest child. The parser's own recursion limit bounds the
/// input, so this recursion is bounded too.
fn depth(value: &Value) -> usize {
    match value {
        Value::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        Value::Object(map) => 1 + map.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Non-empty, at most 2,000 bytes, parses in the shared language.
fn condition_ok(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_CONDITION_BYTES && spot_expr::parse(text).is_ok()
}

/// 1..=64 bytes of `[A-Za-z0-9_/+-]`.
fn zone_ok(zone: &str) -> bool {
    !zone.is_empty()
        && zone.len() <= MAX_ZONE_BYTES
        && zone
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/' | b'+' | b'-'))
}
