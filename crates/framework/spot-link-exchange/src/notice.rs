//! The delivery notice feed (ADR-058 rule 9).
//!
//! A notice says only that something arrived: `{link, topic, message_id, conversation, source,
//! received_at}`. It carries no payload and starts no business logic. **The feed is the only
//! way other subsystems (the process engine) learn of arrivals**; the receive path emits no
//! document event.
//!
//! Delivery to consumers is at least once: [`NoticeFeed::read_from`] returns what is after a
//! cursor and the consumer calls [`NoticeFeed::ack`] when it has handled it. A consumer that
//! crashes between handling and acking reads the notice again, so handlers are idempotent on
//! `message_id`.

use serde::{Deserialize, Serialize};
use spot_tenant_link::{LinkId, Party, Topic};

use crate::error::Result;
use crate::ids::{Conversation, MessageId};

/// A notice before the feed numbers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeDraft {
    /// The link the message arrived on: a message id is only unique within its link.
    pub link: LinkId,
    /// The topic that arrived.
    pub topic: Topic,
    /// The message that arrived.
    pub message_id: MessageId,
    /// Its conversation.
    pub conversation: Conversation,
    /// The sending party.
    pub source: Party,
    /// Unix milliseconds at the receiver.
    pub received_at: u64,
}

/// A numbered notice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    /// The feed position; strictly increasing, starting at 1.
    pub cursor: u64,
    /// The link the message arrived on: a message id is only unique within its link.
    pub link: LinkId,
    /// The topic that arrived.
    pub topic: Topic,
    /// The message that arrived.
    pub message_id: MessageId,
    /// Its conversation.
    pub conversation: Conversation,
    /// The sending party.
    pub source: Party,
    /// Unix milliseconds at the receiver.
    pub received_at: u64,
}

/// The consumer side of the feed. Appending is not part of this trait: a notice is appended only
/// inside the inbox's own atomic commit, so a notice exists exactly when its message was applied.
pub trait NoticeFeed: Send + Sync {
    /// Up to `limit` notices with a cursor above `after`, oldest first. Cursor 0 reads from the
    /// start.
    ///
    /// # Errors
    /// A store failure.
    fn read_from(&self, after: u64, limit: usize) -> Result<Vec<Notice>>;

    /// Records that `consumer` handled everything up to `cursor`. The position only moves
    /// forward: acking an older cursor changes nothing.
    ///
    /// # Errors
    /// A store failure.
    fn ack(&self, consumer: &str, cursor: u64) -> Result<()>;

    /// The cursor `consumer` last acked (0 when it never did).
    ///
    /// # Errors
    /// A store failure.
    fn acked(&self, consumer: &str) -> Result<u64>;
}
