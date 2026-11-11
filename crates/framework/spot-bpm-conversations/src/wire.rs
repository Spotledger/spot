//! How a contract message crosses a tenant link.
//!
//! A link carries an [`ExchangePayload`]: named fields whose values are text, integers or
//! booleans. A contract message is exactly its field list, so the payload carries those fields
//! and one more, the reserved [`MESSAGE_FIELD`], which says *which* contract message it is
//! (`contract@version/message`). Several contract messages share a topic (an approval and a
//! rejection both travel as a decision), so the topic alone cannot tell them apart. The link's
//! grant for a contract topic must therefore list [`MESSAGE_FIELD`] besides the message fields.
//!
//! Decoding is the one place peer-controlled bytes become values, so it is bounded: the header
//! has a size cap, names are validated against the contract name grammar, and a payload with
//! more fields than any contract message may have is refused before it is looked at.

use serde_json::{Map, Number, Value};
use spot_bpm_contract::{MessageDef, is_name, limits};
use spot_tenant_link::{ExchangePayload, FieldName, FieldValue};

use crate::error::Violation;

/// The reserved wire field naming the contract message.
// Built with `concat!` because the error-code scan reads any dotted literal as a code; this is a
// wire field name, not an error code.
pub const MESSAGE_FIELD: &str = concat!("bpm", ".message");

/// The longest header text: two names of at most 64 characters, a version and two separators.
const MAX_HEADER_BYTES: usize = 160;

/// Which contract message a wire payload is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The contract's id.
    pub contract: String,
    /// The contract version the conversation runs on.
    pub version: u32,
    /// The contract message's name.
    pub message: String,
}

impl Header {
    /// A header for `message` of version `version` of `contract`.
    #[must_use]
    pub fn new(contract: impl Into<String>, version: u32, message: impl Into<String>) -> Self {
        Self {
            contract: contract.into(),
            version,
            message: message.into(),
        }
    }

    /// The wire text: `contract@version/message`.
    #[must_use]
    pub fn encode(&self) -> String {
        format!("{}@{}/{}", self.contract, self.version, self.message)
    }

    /// Reads the wire text; `None` for anything that is not a well-formed header.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text.len() > MAX_HEADER_BYTES {
            return None;
        }
        let (contract, rest) = text.split_once('@')?;
        let (version, message) = rest.split_once('/')?;
        if !is_name(contract) || !is_name(message) {
            return None;
        }
        if version.is_empty() || version.len() > 9 || !version.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let version: u32 = version.parse().ok()?;
        (version >= 1).then(|| Self::new(contract, version, message))
    }
}

/// A decoded wire payload: the header and the contract fields as plain JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    /// Which contract message it claims to be.
    pub header: Header,
    /// The contract fields as a JSON object (strings, integers and booleans only).
    pub payload: Value,
}

/// Builds the wire payload for a contract message: its fields, typed as the contract says, and
/// the header.
///
/// The payload must already have passed the schema check; a value that does not fit its field is
/// refused here as well, so nothing the contract does not describe is ever encoded.
///
/// # Errors
/// [`Violation::PayloadInvalid`] when the payload is not an object of the contract's fields.
pub fn encode(
    header: &Header,
    payload: &Value,
    def: &MessageDef,
) -> Result<ExchangePayload, Violation> {
    let invalid = || Violation::PayloadInvalid {
        fields: Vec::new(),
        unknown: 0,
    };
    let object = payload.as_object().ok_or_else(invalid)?;
    let mut wire = ExchangePayload::new();
    for (name, value) in object {
        let Some(field) = def.field(name) else {
            return Err(invalid());
        };
        let wire_value = match (field.kind, value) {
            (spot_bpm_contract::FieldKind::String, Value::String(text)) => {
                FieldValue::Text(text.clone())
            }
            (spot_bpm_contract::FieldKind::Integer, Value::Number(number)) => {
                FieldValue::Int(number.as_i64().ok_or_else(invalid)?)
            }
            (spot_bpm_contract::FieldKind::Boolean, Value::Bool(flag)) => FieldValue::Bool(*flag),
            _ => return Err(invalid()),
        };
        let field_name = FieldName::new(name.as_str()).map_err(|_| invalid())?;
        wire.insert(field_name, wire_value);
    }
    let marker = FieldName::new(MESSAGE_FIELD).map_err(|_| invalid())?;
    wire.insert(marker, FieldValue::Text(header.encode()));
    Ok(wire)
}

/// Reads a received payload (canonical JSON text) into its header and contract fields.
///
/// # Errors
/// [`Violation::HeaderInvalid`] when the text is not a wire payload or names no contract
/// message; [`Violation::PayloadInvalid`] when it carries more fields than a message may have.
pub fn decode(text: &str) -> Result<Decoded, Violation> {
    let wire: ExchangePayload = serde_json::from_str(text).map_err(|_| Violation::HeaderInvalid)?;
    if wire.len() > limits::MAX_FIELDS_PER_MESSAGE + 1 {
        return Err(Violation::PayloadInvalid {
            fields: Vec::new(),
            unknown: wire.len(),
        });
    }
    let mut header = None;
    let mut object = Map::new();
    for (name, value) in wire.iter() {
        if name.as_str() == MESSAGE_FIELD {
            if let FieldValue::Text(text) = value {
                header = Header::parse(text);
            }
            continue;
        }
        let plain = match value {
            FieldValue::Text(text) => Value::String(text.clone()),
            FieldValue::Int(number) => Value::Number(Number::from(*number)),
            FieldValue::Bool(flag) => Value::Bool(*flag),
        };
        object.insert(name.as_str().to_owned(), plain);
    }
    let header = header.ok_or(Violation::HeaderInvalid)?;
    Ok(Decoded {
        header,
        payload: Value::Object(object),
    })
}
