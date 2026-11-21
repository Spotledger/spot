//! Reading the model's optional trigger fields into a spec's parts (WP-BPM-03.T6).
//!
//! Every error here names the node and a fault kind only: the schema text, the condition text
//! and the zone a definition wrote are never echoed.

use spot_bpm_model::{MissedPolicy, NodeId, TriggerConfig};
use spot_scheduler::MissedRunPolicy;

use crate::error::{Error, Result};
use crate::input::check_schema;

/// The start-input schema of a manual or API trigger: JSON text parsed to an object that
/// [`validate_input`](crate::validate_input) can enforce.
pub(crate) fn input_schema(node: &NodeId, text: Option<&str>) -> Result<Option<serde_json::Value>> {
    let Some(text) = text else {
        return Ok(None);
    };
    let invalid = |detail: &str| Error::InputSchemaInvalid {
        node: node.to_string(),
        detail: detail.to_owned(),
    };
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| invalid("the schema text is not valid JSON"))?;
    if !value.is_object() {
        return Err(invalid("the schema is not a JSON object"));
    }
    let problems = check_schema(&value);
    if problems.is_empty() {
        Ok(Some(value))
    } else {
        Err(invalid(&problems.join("; ")))
    }
}

/// The document-event condition: kept as text once it parses.
pub(crate) fn condition(node: &NodeId, text: Option<&str>) -> Result<Option<String>> {
    let Some(text) = text else {
        return Ok(None);
    };
    match spot_webhooks::validate_condition(text) {
        Ok(()) => Ok(Some(text.to_owned())),
        Err(spot_webhooks::Error::InvalidCondition { reason, .. }) => {
            Err(Error::ConditionInvalid {
                node: node.to_string(),
                detail: parse_position(&reason),
            })
        }
        Err(_) => Err(Error::ConditionInvalid {
            node: node.to_string(),
            detail: "the expression does not parse".to_owned(),
        }),
    }
}

/// The parse error's code and offset out of its `"<code> at <offset>: <message>"` text; the
/// message part (which may quote the expression) is dropped.
fn parse_position(reason: &str) -> String {
    let head = reason.split(": ").next().unwrap_or("");
    let mut parts = head.split(" at ");
    match (parts.next(), parts.next(), parts.next()) {
        (Some(code), Some(offset), None)
            if !code.is_empty()
                && code
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
                && offset.chars().all(|c| c.is_ascii_digit()) =>
        {
            format!("{code} at offset {offset}")
        }
        _ => "the expression does not parse".to_owned(),
    }
}

/// The scheduler's missed-run policy a schedule trigger asks for; `run_once` when it says none.
///
/// `skip` is refused: the scheduler's `plan` returns no slot under `Skip` whenever any firing
/// is in the window, an on-time one included, so a `skip` schedule would never fire.
pub(crate) fn missed_policy(node: &NodeId, config: &TriggerConfig) -> Result<MissedRunPolicy> {
    let invalid = |detail: &str| Error::ScheduleInvalid {
        node: node.to_string(),
        detail: detail.to_owned(),
    };
    match config.missed {
        None | Some(MissedPolicy::RunOnce) => Ok(MissedRunPolicy::RunOnce),
        Some(MissedPolicy::Skip) => Err(invalid(
            "the missed-run policy skip is not usable: the scheduler would drop on-time firings",
        )),
        Some(MissedPolicy::CatchUp) => config
            .catch_up_max
            .filter(|max| *max >= 1)
            .map(MissedRunPolicy::CatchUp)
            .ok_or_else(|| invalid("the catch_up policy needs a catch_up_max of at least 1")),
    }
}
