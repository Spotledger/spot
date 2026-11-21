//! The contract text form (ADR-080 section 5.2): one reviewable TOML document
//! per contract version, which is what a bundle section carries and what two
//! participants exchange.
//!
//! The writer is canonical: a fixed key order, one scalar per line, lists in
//! definition order and no whitespace beyond one space around `=`. The same
//! contract therefore always renders to the same bytes, and the
//! [content hash](crate::ProcessContract::content_hash) is computed from the
//! same values rather than from the file.
//!
//! Shapes are checked on read (a missing key, a wrong type, an unknown key and
//! the `next`/`outcome` pair are errors). *Meaning* is not: a duplicate role, a
//! message nobody can reach or a secret field is reported by
//! [`check`](crate::check), which reports every problem at once. A top-level
//! section this build does not know is ignored, so adding one later (a signature
//! scheme, a carrier hint) is a pure addition.

use toml::Value;

use spot_bpm_model::tomlutil::quote;

use crate::error::TextError;
use crate::model::{
    Field, FieldKind, MessageDef, Next, Outcome, OutcomeKind, Participant, ParticipantKind,
    ProcessContract, Signature, TimeoutRule, Transition,
};

/// Serialises a contract version to its canonical TOML text.
#[must_use]
pub fn to_text(contract: &ProcessContract) -> String {
    let mut out = String::new();
    out.push_str(
        "# Spotledger process contract (spot-bpm-contract). Canonical: one scalar per line, a fixed key order.\n",
    );
    line(&mut out, "id", &quote(&contract.id));
    line(&mut out, "version", &contract.version.to_string());
    if let Some(title) = &contract.title {
        line(&mut out, "title", &quote(title));
    }

    for participant in &contract.participants {
        out.push('\n');
        out.push_str("[[participants]]\n");
        line(&mut out, "role", &quote(&participant.role));
        line(&mut out, "kind", &quote(participant.kind.as_str()));
    }

    for message in &contract.messages {
        out.push('\n');
        out.push_str("[[messages]]\n");
        line(&mut out, "name", &quote(&message.name));
        line(&mut out, "topic", &quote(&message.topic));
        line(
            &mut out,
            "schema_version",
            &message.schema_version.to_string(),
        );
        line(&mut out, "from", &quote(&message.from));
        line(&mut out, "to", &quote(&message.to));
        for field in &message.fields {
            out.push('\n');
            out.push_str("[[messages.fields]]\n");
            line(&mut out, "name", &quote(&field.name));
            line(&mut out, "kind", &quote(field.kind.as_str()));
            line(
                &mut out,
                "required",
                if field.required { "true" } else { "false" },
            );
            if let Some(max_len) = field.max_len {
                line(&mut out, "max_len", &max_len.to_string());
            }
        }
    }

    for transition in &contract.order {
        out.push('\n');
        out.push_str("[[order]]\n");
        line(&mut out, "by", &quote(&transition.by));
        if let Some(after) = &transition.after {
            line(&mut out, "after", &quote(after));
        }
        match &transition.next {
            Next::Message(name) => line(&mut out, "next", &quote(name)),
            Next::Outcome(name) => line(&mut out, "outcome", &quote(name)),
        }
        if let Some(bound) = transition.bound {
            line(&mut out, "bound", &bound.to_string());
        }
    }

    for outcome in &contract.outcomes {
        out.push('\n');
        out.push_str("[[outcomes]]\n");
        line(&mut out, "name", &quote(&outcome.name));
        line(&mut out, "kind", &quote(outcome.kind.as_str()));
    }

    for timeout in &contract.timeouts {
        out.push('\n');
        out.push_str("[[timeouts]]\n");
        line(&mut out, "message", &quote(&timeout.message));
        line(&mut out, "by", &quote(&timeout.by));
        line(&mut out, "seconds", &timeout.seconds.to_string());
        if let Some(outcome) = &timeout.outcome {
            line(&mut out, "outcome", &quote(outcome));
        }
    }

    for signature in &contract.signatures {
        out.push('\n');
        out.push_str("[[signatures]]\n");
        line(&mut out, "role", &quote(&signature.role));
        line(&mut out, "algorithm", &quote(&signature.algorithm));
        line(&mut out, "key_id", &quote(&signature.key_id));
        line(&mut out, "signed_hash", &quote(&signature.signed_hash));
        line(&mut out, "signature", &quote(&signature.signature));
    }

    out
}

/// Appends one `key = value` line.
fn line(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(" = ");
    out.push_str(value);
    out.push('\n');
}

/// Parses a contract version from TOML text.
///
/// # Errors
/// [`TextError::TooLarge`] when the text or the contract in it passes a bound
/// of [`limits`](crate::limits) (the text is refused before it is parsed),
/// [`TextError::Toml`] when the text is not well-formed and
/// [`TextError::Shape`] when it is well-formed but not a contract document.
pub fn from_text(text: &str) -> Result<ProcessContract, TextError> {
    if text.len() > crate::limits::MAX_TEXT_BYTES {
        return Err(TextError::TooLarge {
            message: format!(
                "the text is {} bytes, more than the {} allowed",
                text.len(),
                crate::limits::MAX_TEXT_BYTES
            ),
        });
    }
    let value: Value = toml::from_str(text).map_err(|e| TextError::Toml {
        message: e.to_string(),
    })?;
    let table = value
        .as_table()
        .ok_or_else(|| TextError::shape("the document is not a TOML table"))?;
    let contract = ProcessContract {
        id: string(table, "id")?,
        version: u32_value(table, "version")?,
        title: optional_string(table, "title")?,
        participants: parse_participants(table.get("participants"))?,
        messages: parse_messages(table.get("messages"))?,
        order: parse_order(table.get("order"))?,
        outcomes: parse_outcomes(table.get("outcomes"))?,
        timeouts: parse_timeouts(table.get("timeouts"))?,
        signatures: parse_signatures(table.get("signatures"))?,
    };
    if let Some(problem) = crate::limits::exceeded(&contract).into_iter().next() {
        return Err(TextError::TooLarge {
            message: problem.message,
        });
    }
    Ok(contract)
}

/// A required string field.
fn string(table: &toml::Table, key: &str) -> Result<String, TextError> {
    table
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| TextError::shape(format!("`{key}` must be a string")))
}

/// An optional string field.
fn optional_string(table: &toml::Table, key: &str) -> Result<Option<String>, TextError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(TextError::shape(format!("`{key}` must be a string"))),
    }
}

/// A required `u32` field.
fn u32_value(table: &toml::Table, key: &str) -> Result<u32, TextError> {
    let raw = table
        .get(key)
        .and_then(Value::as_integer)
        .ok_or_else(|| TextError::shape(format!("`{key}` must be an integer")))?;
    u32::try_from(raw).map_err(|_| TextError::shape(format!("`{key}` is out of range")))
}

/// An optional `u32` field.
fn optional_u32(table: &toml::Table, key: &str) -> Result<Option<u32>, TextError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(raw)) => u32::try_from(*raw)
            .map(Some)
            .map_err(|_| TextError::shape(format!("`{key}` is out of range"))),
        Some(_) => Err(TextError::shape(format!("`{key}` must be an integer"))),
    }
}

/// An optional `u64` field.
fn optional_u64(table: &toml::Table, key: &str) -> Result<Option<u64>, TextError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(raw)) => u64::try_from(*raw)
            .map(Some)
            .map_err(|_| TextError::shape(format!("`{key}` is out of range"))),
        Some(_) => Err(TextError::shape(format!("`{key}` must be an integer"))),
    }
}

/// A required `u64` field.
fn u64_value(table: &toml::Table, key: &str) -> Result<u64, TextError> {
    optional_u64(table, key)?.ok_or_else(|| TextError::shape(format!("`{key}` must be an integer")))
}

/// An optional boolean field, defaulting to `true` (a field that may cross is
/// expected to be present).
fn required_flag(table: &toml::Table, key: &str) -> Result<bool, TextError> {
    match table.get(key) {
        None => Ok(true),
        Some(Value::Boolean(value)) => Ok(*value),
        Some(_) => Err(TextError::shape(format!("`{key}` must be a boolean"))),
    }
}

/// Rejects a table key that is not in `allowed` (a typo, not a future field).
fn reject_extra(table: &toml::Table, allowed: &[&str], where_: &str) -> Result<(), TextError> {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(TextError::shape(format!("unknown key `{key}` in {where_}")));
        }
    }
    Ok(())
}

/// Parses the `participants` array.
fn parse_participants(value: Option<&Value>) -> Result<Vec<Participant>, TextError> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(TextError::shape(
                "`participants` must be an array of tables",
            ));
        }
    };
    let mut participants = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let table = item
            .as_table()
            .ok_or_else(|| TextError::shape(format!("participants[{i}] is not a table")))?;
        reject_extra(table, &["role", "kind"], &format!("participants[{i}]"))?;
        let role = string(table, "role")?;
        let kind = match optional_string(table, "kind")?.as_deref() {
            None | Some("tenant") => ParticipantKind::Tenant,
            Some("external") => ParticipantKind::External,
            Some(other) => {
                return Err(TextError::shape(format!(
                    "participants[{i}]: unknown participant kind `{other}`"
                )));
            }
        };
        participants.push(Participant::new(role, kind));
    }
    Ok(participants)
}

/// Parses the `messages` array, with each message's nested `fields` array.
fn parse_messages(value: Option<&Value>) -> Result<Vec<MessageDef>, TextError> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(TextError::shape("`messages` must be an array of tables")),
    };
    let mut messages = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let table = item
            .as_table()
            .ok_or_else(|| TextError::shape(format!("messages[{i}] is not a table")))?;
        reject_extra(
            table,
            &["name", "topic", "schema_version", "from", "to", "fields"],
            &format!("messages[{i}]"),
        )?;
        messages.push(MessageDef {
            name: string(table, "name")?,
            topic: string(table, "topic")?,
            schema_version: u32_value(table, "schema_version")?,
            from: string(table, "from")?,
            to: string(table, "to")?,
            fields: parse_fields(table.get("fields"), i)?,
        });
    }
    Ok(messages)
}

/// Parses one message's `fields` array.
fn parse_fields(value: Option<&Value>, message: usize) -> Result<Vec<Field>, TextError> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(TextError::shape(format!(
                "messages[{message}].fields must be an array of tables"
            )));
        }
    };
    let mut fields = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let where_ = format!("messages[{message}].fields[{i}]");
        let table = item
            .as_table()
            .ok_or_else(|| TextError::shape(format!("{where_} is not a table")))?;
        reject_extra(table, &["name", "kind", "required", "max_len"], &where_)?;
        let kind = match string(table, "kind")?.as_str() {
            "string" => FieldKind::String,
            "integer" => FieldKind::Integer,
            "boolean" => FieldKind::Boolean,
            other => {
                return Err(TextError::shape(format!(
                    "{where_}: unknown field kind `{other}`"
                )));
            }
        };
        fields.push(Field {
            name: string(table, "name")?,
            kind,
            required: required_flag(table, "required")?,
            max_len: optional_u64(table, "max_len")?,
        });
    }
    Ok(fields)
}

/// Parses the `order` array.
fn parse_order(value: Option<&Value>) -> Result<Vec<Transition>, TextError> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(TextError::shape("`order` must be an array of tables")),
    };
    let mut order = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let where_ = format!("order[{i}]");
        let table = item
            .as_table()
            .ok_or_else(|| TextError::shape(format!("{where_} is not a table")))?;
        reject_extra(table, &["by", "after", "next", "outcome", "bound"], &where_)?;
        let by = string(table, "by")?;
        let after = optional_string(table, "after")?;
        let next = match (
            optional_string(table, "next")?,
            optional_string(table, "outcome")?,
        ) {
            (Some(next), None) => Next::Message(next),
            (None, Some(outcome)) => Next::Outcome(outcome),
            (Some(_), Some(_)) => {
                return Err(TextError::shape(format!(
                    "{where_}: `next` and `outcome` are mutually exclusive"
                )));
            }
            (None, None) => {
                return Err(TextError::shape(format!(
                    "{where_}: one of `next` or `outcome` is required"
                )));
            }
        };
        order.push(Transition {
            by,
            after,
            next,
            bound: optional_u32(table, "bound")?,
        });
    }
    Ok(order)
}

/// Parses the `outcomes` array.
fn parse_outcomes(value: Option<&Value>) -> Result<Vec<Outcome>, TextError> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(TextError::shape("`outcomes` must be an array of tables")),
    };
    let mut outcomes = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let table = item
            .as_table()
            .ok_or_else(|| TextError::shape(format!("outcomes[{i}] is not a table")))?;
        reject_extra(table, &["name", "kind"], &format!("outcomes[{i}]"))?;
        let kind = match optional_string(table, "kind")?.as_deref() {
            None | Some("completed") => OutcomeKind::Completed,
            Some("cancelled") => OutcomeKind::Cancelled,
            Some("failed") => OutcomeKind::Failed,
            Some("needs_attention") => OutcomeKind::NeedsAttention,
            Some(other) => {
                return Err(TextError::shape(format!(
                    "outcomes[{i}]: unknown outcome kind `{other}`"
                )));
            }
        };
        outcomes.push(Outcome::new(string(table, "name")?, kind));
    }
    Ok(outcomes)
}

/// Parses the `timeouts` array.
fn parse_timeouts(value: Option<&Value>) -> Result<Vec<TimeoutRule>, TextError> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(TextError::shape("`timeouts` must be an array of tables")),
    };
    let mut timeouts = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let table = item
            .as_table()
            .ok_or_else(|| TextError::shape(format!("timeouts[{i}] is not a table")))?;
        reject_extra(
            table,
            &["message", "by", "seconds", "outcome"],
            &format!("timeouts[{i}]"),
        )?;
        timeouts.push(TimeoutRule {
            message: string(table, "message")?,
            by: string(table, "by")?,
            seconds: u64_value(table, "seconds")?,
            outcome: optional_string(table, "outcome")?,
        });
    }
    Ok(timeouts)
}

/// Parses the `signatures` array.
fn parse_signatures(value: Option<&Value>) -> Result<Vec<Signature>, TextError> {
    let items = match value {
        None => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(TextError::shape("`signatures` must be an array of tables")),
    };
    let mut signatures = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let table = item
            .as_table()
            .ok_or_else(|| TextError::shape(format!("signatures[{i}] is not a table")))?;
        reject_extra(
            table,
            &["role", "algorithm", "key_id", "signed_hash", "signature"],
            &format!("signatures[{i}]"),
        )?;
        signatures.push(Signature {
            role: string(table, "role")?,
            algorithm: string(table, "algorithm")?,
            key_id: string(table, "key_id")?,
            signed_hash: string(table, "signed_hash")?,
            signature: string(table, "signature")?,
        });
    }
    Ok(signatures)
}
