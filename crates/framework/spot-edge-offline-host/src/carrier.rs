//! The typed carrier messages: lease requests and answers travel on the existing sync stream as
//! `sys_`-class records, never on a new HTTP path.
//!
//! The wire (`spot-sync-http`) has no request/answer message kind; these are the payloads of the
//! `sys_`-class record kinds the lead registers (see the task report). The node's identity is
//! **never** in the message: the hub takes it from the authenticated connection.

use serde::{Deserialize, Serialize};
use spot_offline_identity::Message;
use spot_offline_identity::signing::SignedNodeRequest;

pub use crate::kinds::{LEASE_ANSWER_KIND, LEASE_REQUEST_KIND, SNAPSHOT_KIND};

/// An edge asks for a range. `request_id` makes the re-request idempotent: the hub returns the
/// original range for a repeated id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRequestMsg {
    /// The envelope id (unique per request, reused on a resend).
    pub request_id: String,
    /// The series.
    pub series: String,
    /// How many numbers are wanted.
    pub size: u64,
}

/// The hub's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LeaseAnswerMsg {
    /// A range was granted.
    Granted {
        /// The request answered.
        request_id: String,
        /// The series.
        series: String,
        /// First number.
        from: u64,
        /// Last number.
        to: u64,
        /// The lease id.
        lease_id: u64,
    },
    /// Refused; the code says why (`lease_grants.*`), never a secret.
    Refused {
        /// The request answered.
        request_id: String,
        /// The stable refusal code.
        code: String,
    },
}

impl LeaseAnswerMsg {
    /// The request this answers.
    pub fn request_id(&self) -> &str {
        match self {
            Self::Granted { request_id, .. } | Self::Refused { request_id, .. } => request_id,
        }
    }
}

/// The permission snapshot exchange, both directions, under [`SNAPSHOT_KIND`].
///
/// The node's identity is never in the message (the hub takes it from the connection). The
/// request is the library's **signed** node request: it carries the node's report of what it
/// holds (so the hub can see a replayed image) and the node key's signature over it; the hub
/// verifies both (`ReplayGuard::admit`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // a wire message: boxing a variant would change nothing on the wire
pub enum SnapshotMsg {
    /// The edge asks for a snapshot or a timestamp.
    Request {
        /// The envelope id (reused on a resend).
        request_id: String,
        /// The signed node request, with the report.
        signed: SignedNodeRequest,
    },
    /// The hub answers with a signed message.
    Issued {
        /// The request answered.
        request_id: String,
        /// The signed snapshot or timestamp.
        message: Message,
        /// The signed hub time that is paired with a snapshot (it carries the fresh nonce; the
        /// snapshot carries none). Accepted right after `message`.
        #[serde(default)]
        companion: Option<Message>,
    },
    /// The hub refuses; the code says why, never a secret.
    Refused {
        /// The request answered.
        request_id: String,
        /// The stable refusal code.
        code: String,
    },
}

impl SnapshotMsg {
    /// The request id.
    pub fn request_id(&self) -> &str {
        match self {
            Self::Request { request_id, .. }
            | Self::Issued { request_id, .. }
            | Self::Refused { request_id, .. } => request_id,
        }
    }
}

/// Encodes a message as a record payload.
///
/// # Errors
///
/// A description when it does not encode.
pub fn encode<T: Serialize>(message: &T) -> Result<String, String> {
    serde_json::to_string(message).map_err(|_| "the message did not encode".to_owned())
}

/// Decodes a record payload.
///
/// # Errors
///
/// A description (never the payload) when it does not parse.
pub fn decode<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T, String> {
    serde_json::from_str(payload).map_err(|_| "the message did not parse".to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn messages_round_trip_and_carry_no_node_identity() {
        let request = LeaseRequestMsg {
            request_id: "r1".into(),
            series: "INV".into(),
            size: 1_000,
        };
        let text = encode(&request).unwrap();
        assert!(!text.contains("node"));
        assert_eq!(decode::<LeaseRequestMsg>(&text).unwrap(), request);
        let answer = LeaseAnswerMsg::Granted {
            request_id: "r1".into(),
            series: "INV".into(),
            from: 1_001,
            to: 2_000,
            lease_id: 7,
        };
        assert_eq!(
            decode::<LeaseAnswerMsg>(&encode(&answer).unwrap()).unwrap(),
            answer
        );
        assert_eq!(answer.request_id(), "r1");
        assert!(decode::<LeaseAnswerMsg>("junk").is_err());
    }
}
