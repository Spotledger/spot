//! The JSON bodies that cross a link. Every body is bounded before it is parsed, and the
//! payload-carrying types are the exchange's own signed [`Message`] and [`Receipt`]: this crate
//! adds no second encoding of either.

use serde::{Deserialize, Serialize};
use spot_link_exchange::{Message, MessageId, Receipt, ReceiptEffect, ReceiveStatus};

/// The content type of every body.
pub const CONTENT_TYPE: &str = "application/json";

/// What became of a pushed message, as the receiver tells the sender.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireStatus {
    /// Stored in order (a message topic) or a slot filled.
    Applied,
    /// A record waiting for the apply path.
    PendingApply,
    /// Held behind a gap.
    Parked,
    /// Already known; the earlier outcome is repeated in the receipts.
    Duplicate,
    /// Refused; the receipt carries the signed code.
    Rejected,
}

impl WireStatus {
    /// The wire status of a receive status; `Busy` has none (it is an error, no receipt).
    #[must_use]
    pub fn of(status: ReceiveStatus) -> Option<Self> {
        match status {
            ReceiveStatus::Applied => Some(Self::Applied),
            ReceiveStatus::PendingApply => Some(Self::PendingApply),
            ReceiveStatus::Parked => Some(Self::Parked),
            ReceiveStatus::Duplicate => Some(Self::Duplicate),
            ReceiveStatus::Rejected(_) => Some(Self::Rejected),
            ReceiveStatus::Busy => None,
        }
    }
}

/// The answer to `POST /link/v1/{link}/messages`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiveReply {
    /// What became of the message.
    pub status: WireStatus,
    /// Receipts to apply on the sender, signed by the receiver; they include receipts for parked
    /// messages this arrival released.
    pub receipts: Vec<Receipt>,
}

/// The answer to `GET /link/v1/{link}/mailbox`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailboxReply {
    /// Messages for the authenticated peer, oldest sequence first. Empty when the long-poll
    /// ended with nothing to send.
    pub messages: Vec<Message>,
}

/// The body of `POST /link/v1/{link}/acks`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckRequest {
    /// Receipts the puller signed for messages it took from the mailbox.
    pub receipts: Vec<Receipt>,
}

/// What a receipt did on the mailbox side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireEffect {
    /// The message moved to a new state.
    Moved,
    /// The tombstone of a slot was confirmed.
    FillerConfirmed,
    /// Nothing changed: a stale or repeated receipt.
    Ignored,
    /// The mailbox never sent this message.
    Unknown,
}

impl WireEffect {
    /// The wire effect of a receipt effect.
    #[must_use]
    pub fn of(effect: ReceiptEffect) -> Self {
        match effect {
            ReceiptEffect::Moved(_) => Self::Moved,
            ReceiptEffect::FillerConfirmed => Self::FillerConfirmed,
            ReceiptEffect::Ignored => Self::Ignored,
        }
    }
}

/// One receipt's result.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckResult {
    /// The message the receipt was about.
    pub message: MessageId,
    /// What it did.
    pub effect: WireEffect,
}

/// The answer to `POST /link/v1/{link}/acks`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckReply {
    /// One result per receipt, in order.
    pub results: Vec<AckResult>,
}

/// The answer to an accepted handshake message or revoke. Nothing but the fact: no echo of the
/// message, no key, nonce or signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntakeReply {
    /// Always `true` on a `200`; a refusal is an error body.
    pub accepted: bool,
}
