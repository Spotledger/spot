//! The `jsonb` columns: the composite parts of an entry that no column of their own carries.
//!
//! An episode's actions, a procedure's steps and confirmations, and a fact's value are small,
//! ordered, nested structures. They are stored as `jsonb` so the table keeps one row per entry
//! while the structure stays queryable, and they are **encoded here from the domain types and
//! decoded back through their constructors**, never round-tripped with an opaque serialiser: the
//! domain types deliberately implement `Serialize` and not `Deserialize`, so a stored document
//! cannot conjure a value the type's rules would refuse.
//!
//! Every free-text field of these structures is a [`Summary`], which only a [`Sanitiser`]
//! produces; decoding therefore re-runs the sanitiser, and a row whose text would no longer pass
//! is refused rather than handed to a caller. The redaction *history* is not stored: text keeps
//! its `[redacted:…]` markers (so it reads the same), and a second scan of that text finds nothing
//! left to redact, which is `spot-agent-memory`'s documented idempotence.

use serde_json::{Map, Value};
use spot_agent_memory::{
    ActionRef, Confirmation, EpisodeAggregate, EpisodeId, FactValue, MemoryError, ParamValue,
    ProcedureStep, RemedyId, Sanitiser, Summary, Symptom,
};
use std::collections::BTreeMap;

use crate::codec::{corrupt, outcome_from_text, outcome_to_text, tier_from_i16, tier_to_i16};

/// The JSON name `ParamValue` and `FactValue` give their variant tag, matching the crate's own
/// serde form (`{"kind":"integer","value":3}`).
const TAG: &str = "kind";
/// The JSON name of the tagged value.
const CONTENT: &str = "value";

/// One object field, or a store failure naming the column.
fn field<'a>(
    column: &str,
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Value, MemoryError> {
    object.get(name).ok_or_else(|| corrupt(column))
}

/// One textual field.
fn field_text<'a>(
    column: &str,
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, MemoryError> {
    field(column, object, name)?
        .as_str()
        .ok_or_else(|| corrupt(column))
}

/// One integer field.
fn integer(column: &str, object: &Map<String, Value>, name: &str) -> Result<i64, MemoryError> {
    field(column, object, name)?
        .as_i64()
        .ok_or_else(|| corrupt(column))
}

/// The object a stored column holds.
fn field_object<'a>(column: &str, value: &'a Value) -> Result<&'a Map<String, Value>, MemoryError> {
    value.as_object().ok_or_else(|| corrupt(column))
}

/// A JSON array's members, each of which must be an object.
fn objects<'a>(column: &str, value: &'a Value) -> Result<&'a [Value], MemoryError> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| corrupt(column))
}

/// The JSON text to bind for `value`, rendered compactly.
fn render(column: &str, value: &Value) -> Result<String, MemoryError> {
    serde_json::to_string(value).map_err(|_| corrupt(column))
}

/// The value a stored column's text parses as.
fn parse(column: &str, text: &str) -> Result<Value, MemoryError> {
    serde_json::from_str(text).map_err(|_| corrupt(column))
}

/// A summary's text, scanned again on the way out.
fn summary(column: &str, sanitiser: &Sanitiser, text: &str) -> Result<Summary, MemoryError> {
    sanitiser.check(text).map_err(|error| MemoryError::Store {
        detail: format!(
            "a stored row could not be decoded at `{column}`: the text no longer passes the sanitiser ({})",
            error.class().map_or("shape", spot_agent_memory::FieldClass::as_str)
        ),
    })
}

/// An episode's actions as `jsonb` text.
///
/// # Errors
///
/// A store failure when the structure cannot be rendered (it cannot: every field is a string, an
/// integer or null).
pub(crate) fn actions_to_json(actions: &[ActionRef]) -> Result<String, MemoryError> {
    let rendered: Vec<Value> = actions
        .iter()
        .map(|action| {
            let mut object = Map::new();
            object.insert("audit".to_owned(), Value::String(action.audit.to_string()));
            object.insert("tier".to_owned(), Value::from(tier_to_i16(action.tier)));
            object.insert(
                "remedy".to_owned(),
                action
                    .remedy
                    .as_ref()
                    .map_or(Value::Null, |remedy| Value::String(remedy.to_string())),
            );
            Value::Object(object)
        })
        .collect();
    render("actions", &Value::Array(rendered))
}

/// An episode's actions, read back from `jsonb` text.
///
/// # Errors
///
/// A store failure when a member is not an action this crate can rebuild.
pub(crate) fn actions_from_json(column: &str, text: &str) -> Result<Vec<ActionRef>, MemoryError> {
    let value = parse(column, text)?;
    let mut actions = Vec::new();
    for member in objects(column, &value)? {
        let object = field_object(column, member)?;
        let audit = crate::codec::audit_from_text(column, field_text(column, object, "audit")?)?;
        let tier = tier_from_i16(
            column,
            i16::try_from(integer(column, object, "tier")?).map_err(|_| corrupt(column))?,
        )?;
        let remedy = match field(column, object, "remedy")? {
            Value::Null => None,
            Value::String(id) => Some(crate::codec::remedy_from_text(column, id)?),
            _ => return Err(corrupt(column)),
        };
        actions.push(ActionRef {
            audit,
            tier,
            remedy,
        });
    }
    Ok(actions)
}

/// One parameter value as the tagged object the domain type's own serde form uses.
///
/// # Errors
///
/// A store failure when the value cannot be rendered.
pub(crate) fn param_to_value(param: &ParamValue) -> Result<Value, MemoryError> {
    let mut object = Map::new();
    match param {
        ParamValue::Text(summary) => {
            object.insert(TAG.to_owned(), Value::String("text".to_owned()));
            object.insert(
                CONTENT.to_owned(),
                Value::String(summary.as_str().to_owned()),
            );
        }
        ParamValue::Integer(value) => {
            object.insert(TAG.to_owned(), Value::String("integer".to_owned()));
            object.insert(CONTENT.to_owned(), Value::from(*value));
        }
        ParamValue::Bool(value) => {
            object.insert(TAG.to_owned(), Value::String("bool".to_owned()));
            object.insert(CONTENT.to_owned(), Value::Bool(*value));
        }
    }
    Ok(Value::Object(object))
}

/// One parameter value read back, its text scanned again.
///
/// # Errors
///
/// A store failure when the object is not a parameter this crate can rebuild.
pub(crate) fn param_from_value(
    column: &str,
    sanitiser: &Sanitiser,
    value: &Value,
) -> Result<ParamValue, MemoryError> {
    let object = field_object(column, value)?;
    let tag = field_text(column, object, TAG)?;
    let content = field(column, object, CONTENT)?;
    match tag {
        "text" => {
            let text = content.as_str().ok_or_else(|| corrupt(column))?;
            Ok(ParamValue::Text(summary(column, sanitiser, text)?))
        }
        "integer" => Ok(ParamValue::Integer(
            content.as_i64().ok_or_else(|| corrupt(column))?,
        )),
        "bool" => Ok(ParamValue::Bool(
            content.as_bool().ok_or_else(|| corrupt(column))?,
        )),
        _ => Err(corrupt(column)),
    }
}

/// A procedure's steps as `jsonb` text: the remedy and its parameters, in order.
///
/// # Errors
///
/// A store failure when a parameter cannot be rendered.
pub(crate) fn steps_to_json(steps: &[ProcedureStep]) -> Result<String, MemoryError> {
    let mut rendered = Vec::with_capacity(steps.len());
    for step in steps {
        let mut params = Map::new();
        for (name, value) in &step.params {
            params.insert(name.clone(), param_to_value(value)?);
        }
        let mut object = Map::new();
        object.insert("remedy".to_owned(), Value::String(step.remedy.to_string()));
        object.insert("params".to_owned(), Value::Object(params));
        rendered.push(Value::Object(object));
    }
    render("steps", &Value::Array(rendered))
}

/// A procedure's steps, read back.
///
/// # Errors
///
/// A store failure when a member is not a step this crate can rebuild, or a parameter name is
/// empty.
pub(crate) fn steps_from_json(
    column: &str,
    sanitiser: &Sanitiser,
    text: &str,
) -> Result<Vec<ProcedureStep>, MemoryError> {
    let value = parse(column, text)?;
    let mut steps = Vec::new();
    for member in objects(column, &value)? {
        let object = field_object(column, member)?;
        let remedy =
            RemedyId::parse(field_text(column, object, "remedy")?).map_err(|_| corrupt(column))?;
        let params_value = field(column, object, "params")?;
        let params_object = field_object(column, params_value)?;
        let mut params = BTreeMap::new();
        for (name, value) in params_object {
            if name.is_empty() {
                return Err(corrupt(column));
            }
            params.insert(name.clone(), param_from_value(column, sanitiser, value)?);
        }
        steps.push(ProcedureStep { remedy, params });
    }
    Ok(steps)
}

/// A procedure's confirmations as `jsonb` text.
///
/// # Errors
///
/// A store failure when the structure cannot be rendered.
pub(crate) fn confirmations_to_json(confirmations: &[Confirmation]) -> Result<String, MemoryError> {
    let rendered: Vec<Value> = confirmations
        .iter()
        .map(|confirmation| {
            let mut object = Map::new();
            object.insert(
                "episode".to_owned(),
                Value::String(confirmation.episode.to_string()),
            );
            object.insert(
                "outcome".to_owned(),
                Value::String(outcome_to_text(confirmation.outcome).to_owned()),
            );
            object.insert("at_ms".to_owned(), Value::from(confirmation.at_ms));
            Value::Object(object)
        })
        .collect();
    render("confirmations", &Value::Array(rendered))
}

/// A procedure's confirmations, read back.
///
/// # Errors
///
/// A store failure when a member is not a confirmation this crate can rebuild.
pub(crate) fn confirmations_from_json(
    column: &str,
    text: &str,
) -> Result<Vec<Confirmation>, MemoryError> {
    let value = parse(column, text)?;
    let mut confirmations = Vec::new();
    for member in objects(column, &value)? {
        let object = field_object(column, member)?;
        let episode: EpisodeId =
            crate::codec::episode_id_from_text(column, field_text(column, object, "episode")?)?;
        let outcome = outcome_from_text(column, field_text(column, object, "outcome")?)?;
        confirmations.push(Confirmation {
            episode,
            outcome,
            at_ms: integer(column, object, "at_ms")?,
        });
    }
    Ok(confirmations)
}

/// One folded-episode aggregate as a JSON object.
fn aggregate_to_value(aggregate: &EpisodeAggregate) -> Value {
    let mut object = Map::new();
    object.insert(
        "symptom".to_owned(),
        Value::String(aggregate.symptom.to_string()),
    );
    object.insert(
        "outcome".to_owned(),
        Value::String(outcome_to_text(aggregate.outcome).to_owned()),
    );
    object.insert("count".to_owned(), Value::from(aggregate.count));
    object.insert("first_ms".to_owned(), Value::from(aggregate.first_ms));
    object.insert("last_ms".to_owned(), Value::from(aggregate.last_ms));
    Value::Object(object)
}

/// The aggregate a stored object holds.
fn aggregate_from_value(column: &str, value: &Value) -> Result<EpisodeAggregate, MemoryError> {
    let object = field_object(column, value)?;
    let symptom: Symptom =
        crate::codec::symptom_from_text(column, field_text(column, object, "symptom")?)?;
    let outcome = outcome_from_text(column, field_text(column, object, "outcome")?)?;
    let count = field(column, object, "count")?
        .as_u64()
        .ok_or_else(|| corrupt(column))?;
    Ok(EpisodeAggregate {
        symptom,
        outcome,
        count,
        first_ms: integer(column, object, "first_ms")?,
        last_ms: integer(column, object, "last_ms")?,
    })
}

/// A fact's value as `jsonb` text.
///
/// # Errors
///
/// A store failure when the value cannot be rendered.
pub(crate) fn fact_value_to_json(value: &FactValue) -> Result<String, MemoryError> {
    let mut object = Map::new();
    match value {
        FactValue::Text(summary) => {
            object.insert(TAG.to_owned(), Value::String("text".to_owned()));
            object.insert(
                CONTENT.to_owned(),
                Value::String(summary.as_str().to_owned()),
            );
        }
        FactValue::Integer(number) => {
            object.insert(TAG.to_owned(), Value::String("integer".to_owned()));
            object.insert(CONTENT.to_owned(), Value::from(*number));
        }
        FactValue::Boolean(flag) => {
            object.insert(TAG.to_owned(), Value::String("boolean".to_owned()));
            object.insert(CONTENT.to_owned(), Value::Bool(*flag));
        }
        FactValue::Instant(at) => {
            object.insert(TAG.to_owned(), Value::String("instant".to_owned()));
            object.insert(CONTENT.to_owned(), Value::from(*at));
        }
        FactValue::Stats(aggregate) => {
            object.insert(TAG.to_owned(), Value::String("stats".to_owned()));
            object.insert(CONTENT.to_owned(), aggregate_to_value(aggregate));
        }
    }
    render("value", &Value::Object(object))
}

/// A fact's value, read back.
///
/// # Errors
///
/// A store failure when the object is not a fact value this crate can rebuild.
pub(crate) fn fact_value_from_json(
    column: &str,
    sanitiser: &Sanitiser,
    text: &str,
) -> Result<FactValue, MemoryError> {
    let value = parse(column, text)?;
    let object = field_object(column, &value)?;
    let tag = field_text(column, object, TAG)?;
    let content = field(column, object, CONTENT)?;
    match tag {
        "text" => {
            let text = content.as_str().ok_or_else(|| corrupt(column))?;
            Ok(FactValue::Text(summary(column, sanitiser, text)?))
        }
        "integer" => Ok(FactValue::Integer(
            content.as_i64().ok_or_else(|| corrupt(column))?,
        )),
        "boolean" => Ok(FactValue::Boolean(
            content.as_bool().ok_or_else(|| corrupt(column))?,
        )),
        "instant" => Ok(FactValue::Instant(
            content.as_i64().ok_or_else(|| corrupt(column))?,
        )),
        "stats" => Ok(FactValue::Stats(aggregate_from_value(column, content)?)),
        _ => Err(corrupt(column)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use spot_agent_memory::testing::sanitiser;
    use spot_agent_memory::{AuditRef, Outcome, Tier};

    fn action(remedy: Option<&str>) -> ActionRef {
        ActionRef {
            audit: AuditRef::parse("audit-1").unwrap(),
            tier: Tier::T2,
            remedy: remedy.map(RemedyId::new),
        }
    }

    #[test]
    fn an_action_round_trips_with_and_without_a_remedy() {
        let actions = vec![action(Some("job.retry_failed")), action(None)];
        let json = actions_to_json(&actions).unwrap();
        assert_eq!(actions_from_json("actions", &json).unwrap(), actions);
    }

    #[test]
    fn a_step_round_trips_with_text_integer_and_bool_parameters() {
        let sanitiser = sanitiser();
        let step = ProcedureStep {
            remedy: RemedyId::new("job.retry_failed"),
            params: BTreeMap::from([
                (
                    "state".to_owned(),
                    ParamValue::Text(sanitiser.check("running").unwrap()),
                ),
                ("attempts".to_owned(), ParamValue::Integer(3)),
                ("force".to_owned(), ParamValue::Bool(true)),
            ]),
        };
        let json = steps_to_json(std::slice::from_ref(&step)).unwrap();
        let read = steps_from_json("steps", &sanitiser, &json).unwrap();
        assert_eq!(read, vec![step]);
        assert!(json.contains("\"kind\":\"integer\""));
    }

    #[test]
    fn a_confirmation_round_trips() {
        let confirmation = Confirmation {
            episode: EpisodeId::parse("episode.e1").unwrap(),
            outcome: Outcome::Harm,
            at_ms: 5_000,
        };
        let json = confirmations_to_json(std::slice::from_ref(&confirmation)).unwrap();
        assert_eq!(
            confirmations_from_json("confirmations", &json).unwrap(),
            vec![confirmation]
        );
    }

    #[test]
    fn every_fact_value_variant_round_trips() {
        let sanitiser = sanitiser();
        let aggregate = EpisodeAggregate {
            symptom: Symptom::parse("job.stuck_running").unwrap(),
            outcome: Outcome::Resolved,
            count: 4,
            first_ms: 1_000,
            last_ms: 9_000,
        };
        let values = vec![
            FactValue::Text(sanitiser.check("standalone").unwrap()),
            FactValue::Integer(16),
            FactValue::Boolean(false),
            FactValue::Instant(1_700_000_000_000),
            FactValue::Stats(aggregate.clone()),
        ];
        for value in values {
            let json = fact_value_to_json(&value).unwrap();
            assert_eq!(
                fact_value_from_json("value", &sanitiser, &json).unwrap(),
                value
            );
        }
        assert!(
            fact_value_to_json(&FactValue::Stats(aggregate))
                .unwrap()
                .contains("\"stats\"")
        );
    }

    #[test]
    fn a_document_that_is_not_the_expected_shape_is_refused() {
        let sanitiser = sanitiser();
        for text in [
            "{}",
            "[]",
            "[1]",
            "null",
            "{\"kind\":\"decimal\",\"value\":1}",
        ] {
            assert!(
                fact_value_from_json("value", &sanitiser, text).is_err(),
                "`{text}` is not a fact value"
            );
        }
        // An action list and a confirmation list may legitimately be empty (an episode that only
        // recorded a diagnosis read, a procedure nobody has confirmed yet), so an empty array is
        // the one document they accept; anything that is not an array is refused.
        assert_eq!(actions_from_json("actions", "[]").unwrap(), Vec::new());
        assert_eq!(
            confirmations_from_json("confirmations", "[]").unwrap(),
            Vec::new()
        );
        for text in ["{}", "[1]", "null", "{\"kind\":\"decimal\",\"value\":1}"] {
            assert!(
                actions_from_json("actions", text).is_err(),
                "`{text}` is not a list of actions"
            );
            assert!(
                confirmations_from_json("confirmations", text).is_err(),
                "`{text}` is not a list of confirmations"
            );
        }
        assert!(steps_from_json("steps", &sanitiser, "[{\"remedy\":\"Job\"}]").is_err());
    }

    #[test]
    fn a_rendered_structure_never_carries_a_float_for_a_number() {
        let json = fact_value_to_json(&FactValue::Integer(1_500)).unwrap();
        assert_eq!(json, "{\"kind\":\"integer\",\"value\":1500}");
        assert!(!json.contains("1.5"));
    }
}
