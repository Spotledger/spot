//! `spot-bpm-conversations` — conversations across organisations for the Business Process engine
//! (WP-BPM-12.T2, ADR-080 section 5, ADR-058).
//!
//! A process runs inside one tenant. When several organisations take part, each runs its own half
//! and the halves exchange messages over a tenant link under a **process contract**
//! (`spot-bpm-contract`). This crate is what lets a half *talk*: it adds to the engine the two
//! step kinds `send message` and `wait for message`, the trigger that starts a request when a
//! message arrives, the conformance checks that hold every message to the contract, and the view a
//! request shows of its conversation.
//!
//! # The pieces
//!
//! - [`SendHandler`] and [`WaitHandler`] ([`handlers`]): the node handlers, registered with the
//!   engine by [`handlers::register`]. A send validates its payload against the contract message
//!   and the order rule **before** anything is queued, queues through a [`MessageSender`] (the
//!   [`LinkSender`] adapter over the link exchange's outbox), and is idempotent by
//!   `(request, node, attempt)`. It continues once the message is queued, acknowledged or applied,
//!   as the node says. A wait registers `(conversation, message)` with a time limit and completes
//!   exactly once when the message arrives.
//! - [`ConformantInbox`] ([`receive`]): the inbox with the contract in front. A message that is
//!   not a known one, not allowed after the previous one, from the wrong role or outside the
//!   schema is refused with a reason code the sender receives and is held in the quarantine.
//! - [`Conversations`] ([`driver`]): the async driver. It consumes the delivery notice feed (the
//!   message-arrived trigger, [`trigger`]), wakes waits, takes timeouts, continues sends whose
//!   delivery completed and runs the compensation hook when the peer cancels.
//! - [`ConversationView`] ([`view`]): what the request shows: contract version, peer, last message
//!   each way, delivery state of every message and what is stuck. An unreachable peer is a
//!   condition, not an error.
//! - [`ConversationStore`] ([`record`]): the durable memory of a conversation on this side.
//!
//! # How a message is told apart
//!
//! Several contract messages share a link topic, so every payload carries the reserved field
//! [`wire::MESSAGE_FIELD`] (`contract@version/message`). The grant of a contract topic must list
//! it besides the message's fields; a grant that does not stops the send with
//! `bpm_conversations.field_not_granted` rather than losing the field silently.
//!
//! # Security
//!
//! A partner's messages are untrusted input. Everything read from one is bounded (header, field
//! count, payload size from the inbox), validated against the contract, and never echoed: a
//! rejection reason names the rule and, at most, fields the contract itself defines. A message
//! that does not verify against the link is handed straight to the inbox, which audits and drops
//! it, so a forger gets neither a receipt nor a quarantine entry.
//!
//! Three rules keep a partner from turning that into damage:
//!
//! - **Every refusal is audited.** [`GateAudit`] takes one row per refusal (the reason code and
//!   the link, conversation and message ids, never a payload); [`LinkAudit`] writes it into the
//!   link's own chain. The runtime has no default trail, so a gate cannot be wired without one.
//! - **Every count is bounded.** The rejection cache is a [`RejectionCache`], keyed by
//!   `(link, message id)` and bounded in every store; [`LinkLimits`] caps the open conversations
//!   and the quarantined inputs one link may have.
//! - **Nothing a crash lost stays lost.** A message the inbox took is recorded before the next
//!   one is judged, and a send the outbox holds is recorded before anything judges the order.
//!
//! Time is always an argument. No `unsafe`, no floats, no `unwrap`/`expect` outside tests.

#![forbid(unsafe_code)]

pub mod audit;
pub mod driver;
pub mod error;
pub mod handlers;
pub mod ledger;
pub mod limits;
pub mod order;
pub mod receive;
pub mod record;
pub mod resolve;
pub mod runtime;
pub mod sender;
pub mod trigger;
pub mod view;
pub mod wire;

pub use audit::{GateAudit, LinkAudit, MemoryGateAudit, Refusal};
pub use driver::{
    ARRIVAL_CONSUMER, ArrivalReport, Cancellation, CompensationDeferred, CompensationHook,
    Conversations, NoCompensation, TickReport,
};
pub use error::{ConversationError, Result, Violation, codes};
pub use handlers::{
    ConversationHandlers, DEFAULT_PAYLOAD_PATH, SendHandler, WaitHandler, register,
};
pub use limits::{LinkLimits, MAX_HELD_INPUTS, MAX_OPEN_CONVERSATIONS};
pub use receive::{ConformantInbox, ReceiveError};
pub use record::{
    ConversationRecord, ConversationStore, Direction, Ending, Event, MemoryConversationStore,
    RejectionCache, Wait, WaitFor, WaitKind, update,
};
pub use resolve::{Resolution, Resolved, resolve_wait};
pub use runtime::{ContractBook, PeerDirectory, Runtime, StaticPeers};
pub use sender::{
    LinkSender, MessageSender, SendRequest, Sent, SharedLink, SharedSigner, message_id,
};
pub use trigger::{
    Arrival, EngineCaseStarter, KIND_MESSAGE_ARRIVED, MessageArrivedKind, message_arrived_spec,
};
pub use view::{ConversationView, MessageView, ViewThresholds};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-conversations";
