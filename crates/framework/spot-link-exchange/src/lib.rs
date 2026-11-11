//! Exchange over a tenant link (WP-CTL-10.T1; ADR-058 rules 4, 8, 9, 13 and 14; ADR-080).
//!
//! Two kinds of topic cross a link: **record** topics (a record version in the replication wire
//! image, applied through `spot-replica-apply`) and **message** topics (an immutable addressed
//! message kept in the receiver's inbox). Both travel as a signed [`Message`] through the same
//! machinery:
//!
//! * the sender's [`Outbox`] signs each message once, numbers it per conversation, and tracks
//!   it `Queued -> Sent -> Acknowledged -> Applied | Rejected(code) | Expired`;
//! * the receiver's [`Inbox`] authenticates, deduplicates, orders, parks gaps, and stores,
//!   appending a delivery notice to the [`NoticeFeed`] in the same atomic commit;
//! * [`stuck_messages`] turns queues that stopped moving into plain-language conditions.
//!
//! Nothing here reads the clock (every instant is an argument), draws randomness (ids come from
//! an [`IdSource`]), moves a float, runs business logic on receipt, or emits a document event.
//! The transport (WP-CTL-10.T2) and the PostgreSQL stores (T3) plug into the traits.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod audit;
pub mod canonical;
pub mod conditions;
mod encode;
pub mod envelope;
pub mod error;
pub mod ids;
pub mod inbox;
pub mod memory;
pub mod message;
pub mod notice;
pub mod outbox;
pub mod retention;
pub mod state;
pub mod store;
pub mod transport;

pub use canonical::{MAX_PAYLOAD_BYTES, canonicalize, is_canonical};
pub use conditions::{Thresholds, stuck_messages};
pub use envelope::Envelope;
pub use error::{Error, Result, codes};
pub use ids::{Conversation, IdSource, MessageId};
pub use inbox::{
    Inbox, InboxConfig, InputQuarantine, MemoryQuarantine, PendingRecord, QuarantinedInput,
    ReceiveOutcome, ReceiveStatus,
};
pub use memory::{MemoryInbox, MemoryOutbox};
pub use message::{Message, MessageParts, TOMBSTONE_EXPIRES, TOMBSTONE_PAYLOAD};
pub use notice::{Notice, NoticeDraft, NoticeFeed};
pub use outbox::{Enqueued, Outbox, OutboxConfig, ReceiptEffect, Submit};
pub use retention::{DEFAULT_RETENTION_DAYS, Lifetimes, RETENTION_SETTING, Retention};
pub use state::{MessageState, Receipt, ReceiptOutcome, RejectCode};
pub use store::{
    ConversationView, EntryStatus, EventKind, Filler, InboxCommit, InboxEntry, InboxStore,
    LinkUsage, OutboxEntry, OutboxStore, ParkedEntry, ProducerEvent, PurgedPayload, SeenKind,
};
pub use transport::TransportParty;

/// This crate's name, for diagnostics.
pub const CRATE_NAME: &str = "spot-link-exchange";
