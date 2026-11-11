//! `spot-client-core`: the sans-IO client core (ADR-088 option C, ADR-091, ADR-092 section 10.2).
//!
//! One state machine under every client. The host (a browser Worker, a React Native module, a
//! test harness) feeds [`Input`] values to [`Core::handle`] and performs the [`Output`] values
//! it returns. The crate has no DOM, no HTTP client, no clock, no random source and no storage:
//!
//! * time moves only when the host sends [`Input::Tick`];
//! * idempotency keys are minted by the caller, at enqueue, and travel unchanged through retries;
//! * the session token ([`token::SessionToken`]) lives in memory and has no `Serialize`;
//! * the optional `storage` feature adds a persistence capability for the pending queue (never
//!   the token); it is off by default. Stored calls never include an authentication, reveal or
//!   event-consumer path (see `persist_policy`), and the stored queue is tagged with its owner
//!   (`Core::start(site)` plus the signed-in user): a queue of another user or site, an
//!   untagged or an unreadable one is discarded unsent with an [`Event::QueueDiscarded`].
//!   A write under `/api/v1/resource/` that carries a body is stored only when the core has
//!   seen the DocType's metadata (and that of its child tables) in a `GET /api/v1/meta/{doctype}`
//!   reply and it has no `secret` field (`meta_seen`); otherwise it runs from memory only.
//!
//! The conformance corpus in `tools/client-conformance/corpus` runs the same scripts through
//! every implementation of this behaviour.

mod cache;
mod core;
#[cfg(feature = "storage")]
mod meta_seen;
mod path;
#[cfg(feature = "storage")]
mod persist_policy;
mod problem;
mod realtime;
#[cfg(feature = "storage")]
mod storage;
mod token;
mod types;

pub use crate::core::{Core, MAX_SENDS};
pub use crate::problem::Problem;
pub use crate::token::SessionToken;
pub use crate::types::{
    DiscardReason, Event, Header, HttpRequest, Input, Output, RealtimeMsg, Reply, Request,
};
