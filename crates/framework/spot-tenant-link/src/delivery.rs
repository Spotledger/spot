//! What revocation does to messages already on their way (ADR-058 rule 13 read with rule 3).
//!
//! Revocation is effective for new exchanges immediately. It never rewrites what already
//! happened: a message the receiver acknowledged (or applied, or rejected with a reason) stays
//! part of the record. A message still queued or sent is **held revoked**, never delivered; the
//! sender's outbox shows it as held, not as silently dropped.
//!
//! WP-CTL-10 owns the outbox and its states; this module holds the pure rule CTL-09 grants and
//! the exchange layer consults.

use serde::{Deserialize, Serialize};

/// A sending side's view of one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// Not yet sent.
    Queued,
    /// Sent, not acknowledged.
    Sent,
    /// The receiver has stored it.
    Acknowledged,
    /// The receiver accepted it.
    Applied,
    /// The receiver refused it with a reason code.
    Rejected,
    /// Past the topic's lifetime.
    Expired,
}

impl DeliveryState {
    /// The state as metadata text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sent => "sent",
            Self::Acknowledged => "acknowledged",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
        }
    }
}

/// What happens to one message when its link is revoked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevocationOutcome {
    /// The message already reached a decision; it stays as it is.
    Stays,
    /// The message was not delivered; it is held as revoked.
    HeldRevoked,
    /// The message had already expired; nothing changes.
    AlreadyExpired,
}

/// The rule, as one pure function the exchange layer calls.
#[must_use]
pub fn after_revocation(state: DeliveryState) -> RevocationOutcome {
    match state {
        DeliveryState::Queued | DeliveryState::Sent => RevocationOutcome::HeldRevoked,
        DeliveryState::Acknowledged | DeliveryState::Applied | DeliveryState::Rejected => {
            RevocationOutcome::Stays
        }
        DeliveryState::Expired => RevocationOutcome::AlreadyExpired,
    }
}
