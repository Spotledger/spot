//! The connector operation set (WP-INT-06.T1, brief item 6).
//!
//! [`EdiConnector`] exposes the adapter as the four operations a BPM integration node or a
//! connector bundle calls: `parse`, `build`, `validate` and `acknowledge`. Transport (AS2,
//! SFTP, HTTP) is explicitly out of scope: the input and output are text and JSON, in and out
//! of memory. [`EdiConnector::describe`] returns the [`ConnectorInfo`] the host advertises, and
//! [`EdiConnector::call`] runs one [`ConnectorCall`] from the shared connector vocabulary,
//! mapping an [`EdiError`] to a [`ConnectorError`] at the boundary.

use serde::Deserialize;
use serde_json::{Value as Json, json};
use spot_connectors::{
    AuthKind, ConnectorCall, ConnectorError, ConnectorId, ConnectorInfo, ConnectorOutput, Operation,
};

use crate::ack::{AckRequest, TransactionResult, acknowledge};
use crate::error::{EdiError, Result};
use crate::limits::Limits;
use crate::parser::{parse, parse_str};
use crate::schema::{Schema, validate};
use crate::writer::write;

/// The connector's stable id.
pub const CONNECTOR_ID: &str = "spot-edi";

/// The adapter as a connector operation set.
#[derive(Clone, Copy, Debug, Default)]
pub struct EdiConnector {
    /// The limits every parse runs under.
    pub limits: Limits,
}

impl EdiConnector {
    /// A connector with the default limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// The connector's identity and operations, as the host advertises them.
    pub fn describe() -> ConnectorInfo {
        ConnectorInfo {
            name: "EDI adapter".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            operations: vec![
                operation(
                    "parse",
                    "Parse an X12 or EDIFACT interchange into its model.",
                ),
                operation(
                    "build",
                    "Write an interchange model back to canonical text.",
                ),
                operation(
                    "validate",
                    "Validate an interchange or message against a schema.",
                ),
                operation("acknowledge", "Build a 997 or CONTRL acknowledgement."),
            ],
            auth: vec![AuthKind::None],
        }
    }

    /// Runs one operation against `input`, returning the operation's output.
    pub fn invoke(&self, operation: &str, input: &Json) -> Result<Json> {
        match operation {
            "parse" => self.parse(input),
            "build" => self.build(input),
            "validate" => self.validate(input),
            "acknowledge" => self.acknowledge(input),
            other => Err(EdiError::UnknownOperation {
                operation: other.to_owned(),
            }),
        }
    }

    /// Runs one [`ConnectorCall`], mapping errors to the shared connector vocabulary.
    pub fn call(
        &self,
        call: &ConnectorCall,
    ) -> std::result::Result<ConnectorOutput, ConnectorError> {
        self.invoke(&call.operation, &call.input)
            .map(|value| ConnectorOutput { value })
            .map_err(to_connector_error)
    }

    fn parse(&self, input: &Json) -> Result<Json> {
        let request: ParseInput = decode(input)?;
        // A caller may tighten the connector's limits, never raise them.
        let limits = request.limits.map_or(self.limits, |asked| {
            let asked: Limits = asked.into();
            Limits {
                max_bytes: asked.max_bytes.min(self.limits.max_bytes),
                max_segments: asked.max_segments.min(self.limits.max_segments),
                max_elements: asked.max_elements.min(self.limits.max_elements),
            }
        });
        let interchange = parse_str(&request.text, limits)?;
        let envelope = crate::envelope::check(&interchange);
        Ok(json!({
            "interchange": interchange,
            "envelope": envelope,
        }))
    }

    fn build(&self, input: &Json) -> Result<Json> {
        let request: BuildInput = decode(input)?;
        let text = write(&request.interchange)?;
        Ok(json!({ "text": text }))
    }

    fn validate(&self, input: &Json) -> Result<Json> {
        let request: ValidateInput = decode(input)?;
        let schema = Schema::load(&request.schema)?;
        let violations = match (request.text, request.interchange) {
            (Some(text), _) => {
                let interchange = parse_str(&text, self.limits)?;
                crate::schema::validate_interchange(&interchange, &schema)
            }
            (None, Some(interchange)) => {
                let message = interchange.messages().into_iter().next();
                match message {
                    Some(message) => validate(&message, &schema),
                    None => return Err(EdiError::NoMessage),
                }
            }
            (None, None) => {
                return Err(EdiError::BadInput {
                    reason: "validate needs either `text` or `interchange`".into(),
                });
            }
        };
        Ok(json!({ "violations": violations }))
    }

    fn acknowledge(&self, input: &Json) -> Result<Json> {
        let request: AckInput = decode(input)?;
        let interchange = parse_str(&request.text, self.limits)?;
        let ack_request = AckRequest {
            control: request.control,
            date: request.date,
            time: request.time,
            functional_group: request.functional_group,
            group_control: request.group_control,
            results: request.results,
        };
        let ack = acknowledge(&interchange, &ack_request)?;
        Ok(json!({ "text": write(&ack)?, "interchange": ack }))
    }
}

/// Builds one operation descriptor with a permissive JSON schema.
fn operation(name: &str, summary: &str) -> Operation {
    let schema = format!("{{\"type\":\"object\",\"description\":{}}}", json!(summary));
    Operation {
        name: name.to_owned(),
        input_schema: schema.clone(),
        output_schema: schema,
    }
}

/// Decodes a connector operation's input.
fn decode<T: for<'de> Deserialize<'de>>(input: &Json) -> Result<T> {
    serde_json::from_value(input.clone()).map_err(|e| EdiError::BadInput {
        reason: e.to_string(),
    })
}

/// Maps an adapter error onto the shared connector error vocabulary.
pub fn to_connector_error(error: EdiError) -> ConnectorError {
    match error {
        EdiError::UnknownOperation { operation } => ConnectorError::UnknownOperation { operation },
        other => ConnectorError::BadInput {
            reason: other.to_string(),
        },
    }
}

/// The connector id as the shared vocabulary's type.
pub fn connector_id() -> Option<ConnectorId> {
    ConnectorId::new(CONNECTOR_ID)
}

/// The limits an operation's input may set.
#[derive(Clone, Copy, Debug, Deserialize)]
struct LimitsInput {
    max_bytes: usize,
    max_segments: usize,
    max_elements: usize,
}

impl From<LimitsInput> for Limits {
    fn from(input: LimitsInput) -> Self {
        Self {
            max_bytes: input.max_bytes,
            max_segments: input.max_segments,
            max_elements: input.max_elements,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ParseInput {
    text: String,
    #[serde(default)]
    limits: Option<LimitsInput>,
}

#[derive(Debug, Deserialize)]
struct BuildInput {
    interchange: crate::model::Interchange,
}

#[derive(Debug, Deserialize)]
struct ValidateInput {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    interchange: Option<crate::model::Interchange>,
    schema: String,
}

#[derive(Debug, Deserialize)]
struct AckInput {
    text: String,
    control: String,
    date: String,
    time: String,
    #[serde(default)]
    functional_group: Option<String>,
    #[serde(default)]
    group_control: Option<String>,
    #[serde(default)]
    results: Vec<TransactionResult>,
}

/// Parses bytes directly, for a caller that already holds them.
pub fn parse_bytes(input: &[u8], limits: Limits) -> Result<crate::model::Interchange> {
    parse(input, limits)
}
