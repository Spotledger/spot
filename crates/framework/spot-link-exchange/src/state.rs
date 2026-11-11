//! Delivery states, rejection codes and the signed receipt a receiver returns.

use std::fmt;

use serde::{Deserialize, Serialize};
use spot_tenant_link::{DeliveryState, IdentitySigner, LinkId, Party, PeerKey, SignatureBytes};

use crate::encode::{put_field, put_u64};
use crate::error::{Error, Result, codes};
use crate::ids::{Conversation, MessageId};

/// Why a receiver refused a message. The sender sees the code; it is terminal for the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    /// The sent schema version is outside the receiver's granted range.
    VersionUnsupported,
    /// The link does not grant the topic in this direction (or is not active).
    NotGranted,
    /// The message arrived after it expired, or after its slot in the conversation passed.
    OutOfOrderExpired,
    /// The payload is invalid for the topic, or the message contradicts an earlier one.
    PayloadInvalid,
    /// This link already holds as much parked work as its limits allow. The refusal
    /// is per link: one link's flood never refuses another link's messages.
    CapacityExceeded,
}

impl RejectCode {
    /// The stable dotted code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::VersionUnsupported => codes::VERSION_UNSUPPORTED,
            Self::NotGranted => codes::NOT_GRANTED,
            Self::OutOfOrderExpired => codes::OUT_OF_ORDER_EXPIRED,
            Self::PayloadInvalid => codes::PAYLOAD_INVALID,
            Self::CapacityExceeded => codes::CAPACITY_EXCEEDED,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::VersionUnsupported => 1,
            Self::NotGranted => 2,
            Self::OutOfOrderExpired => 3,
            Self::PayloadInvalid => 4,
            Self::CapacityExceeded => 5,
        }
    }
}

impl fmt::Display for RejectCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// The state of one message as its sender sees it (ADR-058 rule 13).
///
/// `Queued -> Sent -> Acknowledged -> Applied`, or `Rejected(code)`, or `Expired`. `Expired`
/// means no confirmation arrived before the message's lifetime ended; it is always visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    /// Waiting to be sent.
    Queued,
    /// Handed to the carrier; no receipt yet.
    Sent,
    /// The receiver holds it but has not applied it yet (parked behind a gap, or a record
    /// waiting for its apply).
    Acknowledged,
    /// The receiver applied it.
    Applied,
    /// The receiver refused it.
    Rejected(RejectCode),
    /// No confirmation arrived before it expired.
    Expired,
}

impl MessageState {
    /// The state as text, for audit and screens.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sent => "sent",
            Self::Acknowledged => "acknowledged",
            Self::Applied => "applied",
            Self::Rejected(_) => "rejected",
            Self::Expired => "expired",
        }
    }

    /// Whether the message can still change state by a receipt or by expiry.
    #[must_use]
    pub fn is_open(self) -> bool {
        matches!(self, Self::Queued | Self::Sent | Self::Acknowledged)
    }

    /// The matching state of the tenant link's delivery vocabulary (used for revocation).
    #[must_use]
    pub fn delivery(self) -> DeliveryState {
        match self {
            Self::Queued => DeliveryState::Queued,
            Self::Sent => DeliveryState::Sent,
            Self::Acknowledged => DeliveryState::Acknowledged,
            Self::Applied => DeliveryState::Applied,
            Self::Rejected(_) => DeliveryState::Rejected,
            Self::Expired => DeliveryState::Expired,
        }
    }
}

/// What a receipt reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptOutcome {
    /// Held by the receiver, not applied yet.
    Acknowledged,
    /// Applied.
    Applied,
    /// Refused.
    Rejected(RejectCode),
}

impl ReceiptOutcome {
    fn encode(self, out: &mut Vec<u8>) {
        match self {
            Self::Acknowledged => out.extend_from_slice(&[1, 0]),
            Self::Applied => out.extend_from_slice(&[2, 0]),
            Self::Rejected(code) => out.extend_from_slice(&[3, code.tag()]),
        }
    }
}

/// The receiver's signed statement about one message. The sender verifies it against the key
/// pinned in the link, so a receipt cannot be forged by anyone but the peer.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    /// The link.
    pub link: LinkId,
    /// The message this is about.
    pub message: MessageId,
    /// Its conversation.
    pub conversation: Conversation,
    /// Its sequence number.
    pub seq: u64,
    /// What happened.
    pub outcome: ReceiptOutcome,
    /// Unix milliseconds on the receiver.
    pub at: u64,
    /// The receiver.
    pub signer: Party,
    /// The receiver's signature.
    pub signature: SignatureBytes,
}

impl fmt::Debug for Receipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receipt")
            .field("message", &self.message)
            .field("seq", &self.seq)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

impl Receipt {
    /// The bytes that are signed.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"spotledger-link-receipt-v1\0");
        out.extend_from_slice(self.link.as_uuid().as_bytes());
        out.extend_from_slice(self.message.bytes());
        put_field(&mut out, self.conversation.as_str().as_bytes());
        put_u64(&mut out, self.seq);
        self.outcome.encode(&mut out);
        put_u64(&mut out, self.at);
        put_field(&mut out, self.signer.canonical().as_bytes());
        out
    }

    /// Signs a receipt as `signer`.
    #[must_use]
    pub fn sign(
        signer: &dyn IdentitySigner,
        link: LinkId,
        message: MessageId,
        conversation: Conversation,
        seq: u64,
        outcome: ReceiptOutcome,
        at: u64,
    ) -> Self {
        let mut receipt = Self {
            link,
            message,
            conversation,
            seq,
            outcome,
            at,
            signer: signer.party(),
            signature: SignatureBytes::from_bytes([0u8; 64]),
        };
        receipt.signature = signer.sign(&receipt.signing_bytes());
        receipt
    }

    /// Checks the signature against the peer's pinned key.
    ///
    /// # Errors
    /// [`Error::BadSignature`] when it does not verify.
    pub fn verify(&self, key: &PeerKey) -> Result<()> {
        key.verify(&self.signing_bytes(), &self.signature)
            .map_err(|_| Error::BadSignature)
    }
}
