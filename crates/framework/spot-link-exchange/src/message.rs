//! The signed message (ADR-058 rule 8): one immutable item addressed along a link.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use spot_tenant_link::{IdentitySigner, LinkId, Party, PeerKey, SignatureBytes, Topic};

use crate::canonical;
use crate::encode::{put_field, put_u32, put_u64};
use crate::error::{Error, Result};
use crate::ids::{Conversation, MessageId};

/// The payload of a tombstone: an empty object.
pub const TOMBSTONE_PAYLOAD: &str = "{}";

/// The expiry of a tombstone: it never expires, because it only fills a slot in a sequence.
pub const TOMBSTONE_EXPIRES: u64 = u64::MAX;

/// What a sender chooses for one message; [`Message::sign`] completes it.
#[derive(Clone, Debug)]
pub struct MessageParts {
    /// The message id.
    pub id: MessageId,
    /// The link it travels on.
    pub link: LinkId,
    /// The conversation.
    pub conversation: Conversation,
    /// The sequence number, from 1, per conversation.
    pub seq: u64,
    /// The topic.
    pub topic: Topic,
    /// The topic schema version.
    pub schema_version: u32,
    /// Unix milliseconds at the sender.
    pub sent_at: u64,
    /// Unix milliseconds after which the receiver refuses it.
    pub expires_at: u64,
    /// Whether this only fills a slot (an expired or rejected message's sequence number).
    pub tombstone: bool,
    /// For a tombstone, the message whose slot it fills.
    pub replaces: Option<MessageId>,
    /// Canonical JSON.
    pub payload: String,
}

/// A message, signed by the sender over every field (ADR-058 rule 8).
///
/// The signature binds the link, the sender, the conversation, the sequence number, the topic,
/// the schema version, both instants and the payload, so none of them can be altered or moved to
/// another link in transit.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    /// The message id (UUIDv7); the receiver deduplicates on it.
    pub id: MessageId,
    /// The link it travels on.
    pub link: LinkId,
    /// The sending party.
    pub sender: Party,
    /// The conversation.
    pub conversation: Conversation,
    /// The sequence number, from 1, per conversation.
    pub seq: u64,
    /// The topic.
    pub topic: Topic,
    /// The topic schema version.
    pub schema_version: u32,
    /// Unix milliseconds at the sender.
    pub sent_at: u64,
    /// Unix milliseconds after which the receiver refuses it.
    pub expires_at: u64,
    /// Whether this only fills a slot in the sequence.
    pub tombstone: bool,
    /// For a tombstone, the message whose slot it fills; the receiver then repeats what became
    /// of that message, so a sender that expired it learns it was applied after all.
    pub replaces: Option<MessageId>,
    /// Canonical JSON, at most [`canonical::MAX_PAYLOAD_BYTES`] bytes.
    pub payload: String,
    /// The sender's signature.
    pub sender_signature: SignatureBytes,
}

impl fmt::Debug for Message {
    /// The payload is business data and never printed; only its length is.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("id", &self.id)
            .field("conversation", &self.conversation)
            .field("seq", &self.seq)
            .field("topic", &self.topic)
            .field("tombstone", &self.tombstone)
            .field("payload_bytes", &self.payload.len())
            .finish_non_exhaustive()
    }
}

impl Message {
    /// Signs `parts` as `signer`.
    ///
    /// # Errors
    /// [`Error::InvalidPayload`] when the payload is not canonical JSON within the size cap, or
    /// a tombstone does not carry the fixed tombstone payload and expiry;
    /// [`Error::InvalidId`] when the sequence number is 0 or the expiry is not after the send.
    pub fn sign(parts: MessageParts, signer: &dyn IdentitySigner) -> Result<Self> {
        if parts.seq == 0 {
            return Err(Error::InvalidId {
                reason: "a sequence number starts at 1",
            });
        }
        if parts.expires_at <= parts.sent_at {
            return Err(Error::InvalidId {
                reason: "a message expires after it is sent",
            });
        }
        if !canonical::is_canonical(&parts.payload) {
            return Err(Error::InvalidPayload {
                reason: "the payload is not canonical JSON",
            });
        }
        if parts.replaces.is_some() && !parts.tombstone {
            return Err(Error::InvalidPayload {
                reason: "only a tombstone replaces a message",
            });
        }
        if parts.tombstone
            && (parts.payload != TOMBSTONE_PAYLOAD || parts.expires_at != TOMBSTONE_EXPIRES)
        {
            return Err(Error::InvalidPayload {
                reason: "a tombstone carries no payload and does not expire",
            });
        }
        let mut message = Self {
            id: parts.id,
            link: parts.link,
            sender: signer.party(),
            conversation: parts.conversation,
            seq: parts.seq,
            topic: parts.topic,
            schema_version: parts.schema_version,
            sent_at: parts.sent_at,
            expires_at: parts.expires_at,
            tombstone: parts.tombstone,
            replaces: parts.replaces,
            payload: parts.payload,
            sender_signature: SignatureBytes::from_bytes([0u8; 64]),
        };
        message.sender_signature = signer.sign(&message.signing_bytes());
        Ok(message)
    }

    /// The bytes that are signed.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.payload.len() + 256);
        out.extend_from_slice(b"spotledger-link-message-v1\0");
        out.extend_from_slice(self.id.bytes());
        out.extend_from_slice(self.link.as_uuid().as_bytes());
        put_field(&mut out, self.sender.canonical().as_bytes());
        put_field(&mut out, self.conversation.as_str().as_bytes());
        put_u64(&mut out, self.seq);
        put_field(&mut out, self.topic.as_str().as_bytes());
        put_u32(&mut out, self.schema_version);
        put_u64(&mut out, self.sent_at);
        put_u64(&mut out, self.expires_at);
        out.push(u8::from(self.tombstone));
        match &self.replaces {
            Some(id) => {
                out.push(1);
                out.extend_from_slice(id.bytes());
            }
            None => out.push(0),
        }
        put_field(&mut out, self.payload.as_bytes());
        out
    }

    /// Checks the signature against the sender's pinned key.
    ///
    /// # Errors
    /// [`Error::BadSignature`] when it does not verify.
    pub fn verify(&self, key: &PeerKey) -> Result<()> {
        key.verify(&self.signing_bytes(), &self.sender_signature)
            .map_err(|_| Error::BadSignature)
    }

    /// The SHA-256 of the payload, for audit and quarantine records that must not hold it.
    #[must_use]
    pub fn payload_digest(&self) -> [u8; 32] {
        Sha256::digest(self.payload.as_bytes()).into()
    }
}
