//! The JSON a peer sees: the outbound body of a message, the inbound body of a message or a
//! business acknowledgement, and the mapping between the peer's own shape and the platform's.
//!
//! The platform envelope going out:
//!
//! ```json
//! {"id": "<uuid>", "conversation": "<name>", "seq": 1, "topic": "<topic>",
//!  "schema_version": 1, "sent_at": 0, "expires_at": 0, "payload": {"<field>": "<value>"}}
//! ```
//!
//! and coming in (`payload` may be flat dotted names or nested objects; both mean the same
//! fields):
//!
//! ```json
//! {"id": "<the peer's id>", "conversation": "<name>", "topic": "<topic>",
//!  "schema_version": 1, "payload": {"<field>": "<value>"}}
//! ```
//!
//! A business acknowledgement of one of our messages (the peer applied or refused it):
//!
//! ```json
//! {"ack": {"message": "<our message id>", "outcome": "applied"}}
//! ```
//!
//! `outcome` is `acknowledged`, `applied` or `rejected` (with `"code"`: one of the exchange's
//! rejection codes). Numbers are integers; money and quantities travel as text (ADR-12).

use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use spot_link_exchange::{Conversation, Message, MessageId, ReceiptOutcome, RejectCode};
use spot_tenant_link::{ExchangePayload, FieldName, FieldValue, LinkId, Topic};

use crate::error::{Error, Result};
use crate::peer::{ExternalPeer, PeerFormat};

const MAX_PEER_ID_BYTES: usize = 128;

/// A message the peer sent us, not yet attributed to a link or given a sequence number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundMessage {
    /// The peer's own id for the message; replays are recognised by it.
    pub peer_id: String,
    /// The conversation.
    pub conversation: Conversation,
    /// The topic.
    pub topic: Topic,
    /// The topic schema version.
    pub schema_version: u32,
    /// The fields.
    pub payload: ExchangePayload,
}

/// The peer's business acknowledgement of one of our messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboundAck {
    /// Our message.
    pub message: MessageId,
    /// What the peer says became of it.
    pub outcome: ReceiptOutcome,
}

/// What an inbound body carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    /// A message for the inbox.
    Message(InboundMessage),
    /// An acknowledgement for the outbox.
    Ack(InboundAck),
}

/// The platform envelope of `message`.
///
/// # Errors
/// [`Error::InvalidBody`] when the stored payload is not a message payload (a signed message
/// on a message topic always is).
pub fn outbound_envelope(message: &Message) -> Result<Value> {
    let typed: ExchangePayload =
        serde_json::from_str(&message.payload).map_err(|_| Error::InvalidBody {
            reason: "the payload is not a message payload",
        })?;
    // The peer sees plain values; the platform's tagged values stay inside.
    let payload = Value::Object(
        typed
            .iter()
            .map(|(name, value)| {
                let plain = match value {
                    FieldValue::Text(text) => Value::String(text.clone()),
                    FieldValue::Int(number) => Value::from(*number),
                    FieldValue::Bool(flag) => Value::Bool(*flag),
                };
                (name.as_str().to_owned(), plain)
            })
            .collect(),
    );
    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(message.id.to_string()));
    map.insert(
        "conversation".to_owned(),
        Value::String(message.conversation.as_str().to_owned()),
    );
    map.insert("seq".to_owned(), Value::from(message.seq));
    map.insert(
        "topic".to_owned(),
        Value::String(message.topic.as_str().to_owned()),
    );
    map.insert(
        "schema_version".to_owned(),
        Value::from(message.schema_version),
    );
    map.insert("sent_at".to_owned(), Value::from(message.sent_at));
    map.insert("expires_at".to_owned(), Value::from(message.expires_at));
    map.insert("payload".to_owned(), payload);
    Ok(Value::Object(map))
}

fn micros(now_ms: u64) -> Result<i64> {
    i64::try_from(now_ms)
        .ok()
        .and_then(|ms| ms.checked_mul(1_000))
        .ok_or(Error::InvalidBody {
            reason: "the instant is out of range",
        })
}

/// The body to send for `message`, in the peer's format. `now_ms` is the mapping's clock.
///
/// # Errors
/// [`Error::MappingFailed`] when a rule of the peer's mapping fails.
pub fn encode_outbound(peer: &ExternalPeer, message: &Message, now_ms: u64) -> Result<Value> {
    let envelope = outbound_envelope(message)?;
    match &peer.format {
        PeerFormat::JsonEnvelope => Ok(envelope),
        PeerFormat::Mapped(spec) => spec
            .apply(&envelope, micros(now_ms)?)
            .map_err(|_| Error::MappingFailed),
    }
}

/// Reads what the peer sent, in the peer's format.
///
/// A top-level `ack` object is always read as an acknowledgement, whatever the format; any
/// other body is mapped (when the peer has a mapping) and read as a message.
///
/// # Errors
/// [`Error::InvalidBody`] for a body that is not a message or acknowledgement this link can
/// carry (not JSON, a missing or malformed field, a float, a nested array), and
/// [`Error::MappingFailed`] when a mapping rule fails.
pub fn decode_inbound(peer: &ExternalPeer, body: &[u8], now_ms: u64) -> Result<Inbound> {
    let raw: Value = serde_json::from_slice(body).map_err(|_| Error::InvalidBody {
        reason: "the body is not JSON",
    })?;
    if let Some(ack) = raw.get("ack") {
        return decode_ack(ack).map(Inbound::Ack);
    }
    let envelope = match &peer.format {
        PeerFormat::JsonEnvelope => raw,
        PeerFormat::Mapped(spec) => spec
            .apply(&raw, micros(now_ms)?)
            .map_err(|_| Error::MappingFailed)?,
    };
    decode_message(&envelope).map(Inbound::Message)
}

fn invalid(reason: &'static str) -> Error {
    Error::InvalidBody { reason }
}

fn decode_ack(ack: &Value) -> Result<InboundAck> {
    let message = ack
        .get("message")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<MessageId>().ok())
        .ok_or(invalid("the acknowledgement names no message"))?;
    let outcome = match ack.get("outcome").and_then(Value::as_str) {
        Some("acknowledged") => ReceiptOutcome::Acknowledged,
        Some("applied") => ReceiptOutcome::Applied,
        Some("rejected") => {
            let code = ack
                .get("code")
                .cloned()
                .and_then(|code| serde_json::from_value::<RejectCode>(code).ok())
                .unwrap_or(RejectCode::PayloadInvalid);
            ReceiptOutcome::Rejected(code)
        }
        _ => return Err(invalid("the acknowledgement outcome is not known")),
    };
    Ok(InboundAck { message, outcome })
}

fn decode_message(envelope: &Value) -> Result<InboundMessage> {
    let peer_id = match envelope.get("id") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) if number.is_u64() || number.is_i64() => number.to_string(),
        _ => return Err(invalid("the message has no id")),
    };
    if peer_id.is_empty()
        || peer_id.len() > MAX_PEER_ID_BYTES
        || peer_id.chars().any(char::is_control)
    {
        return Err(invalid("the message id is 1 to 128 printable characters"));
    }
    let text = |key: &str| envelope.get(key).and_then(Value::as_str);
    let conversation = text("conversation")
        .and_then(|name| Conversation::new(name).ok())
        .ok_or(invalid("the conversation is missing or malformed"))?;
    let topic = text("topic")
        .and_then(|name| Topic::new(name).ok())
        .ok_or(invalid("the topic is missing or malformed"))?;
    let schema_version = envelope
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(invalid("the schema version is missing or malformed"))?;
    let fields = envelope
        .get("payload")
        .and_then(Value::as_object)
        .ok_or(invalid("the payload is not an object"))?;
    let mut payload = ExchangePayload::new();
    for (name, value) in fields {
        flatten(value, name, &mut payload)?;
    }
    Ok(InboundMessage {
        peer_id,
        conversation,
        topic,
        schema_version,
        payload,
    })
}

/// Nested objects become dotted names; a leaf is text, a whole number or a boolean.
fn flatten(value: &Value, name: &str, out: &mut ExchangePayload) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                flatten(child, &format!("{name}.{key}"), out)?;
            }
            Ok(())
        }
        leaf => {
            let field = FieldName::new(name).map_err(|_| invalid("a field name is malformed"))?;
            let value = match leaf {
                Value::String(text) => FieldValue::Text(text.clone()),
                Value::Bool(flag) => FieldValue::Bool(*flag),
                Value::Number(number) => FieldValue::Int(
                    number
                        .as_i64()
                        .ok_or(invalid("a number is not a whole number"))?,
                ),
                _ => return Err(invalid("a field is not text, a whole number or a boolean")),
            };
            out.insert(field, value);
            Ok(())
        }
    }
}

/// The message id a peer's own id becomes on `link`: deterministic, so a replay of the same
/// peer id is recognised by the inbox, and tied to the link, so the same peer id on another
/// link is a different message.
#[must_use]
pub fn external_message_id(link: LinkId, peer_id: &str) -> MessageId {
    let mut hash = Sha256::new();
    hash.update(b"spotledger-external-message-id-v1\0");
    hash.update(link.as_uuid().as_bytes());
    hash.update(peer_id.as_bytes());
    let digest = hash.finalize();
    let mut entropy = [0u8; 10];
    entropy.copy_from_slice(&digest[..10]);
    MessageId::new_v7(0, entropy)
}
