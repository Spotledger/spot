//! `spot-sync-node-host`: the edge half of sync, hosted (WP-RTB-02.T5; ADR-062 sections 3, 4 and
//! 7, ADR-064 section 7, ADR-066, ADR-067 section 3).
//!
//! The pieces below `spot-sync-http` (the wire, `EdgeClient`, the resilience helpers) and
//! `spot-sync-node` (the receiving state machine) exist; nothing ran them against a real
//! database. This crate is the loop that does, inside `spot serve` on a site with
//! `role = "edge"` (`spot-cli`'s `services/sync_node.rs` is the thin module that hosts it).
//!
//! | Piece | What |
//! | --- | --- |
//! | [`NodeLoop`] | hello, pull, apply, acknowledge, push; backoff, breaker, offline-since |
//! | [`HubLink`] | one hub: hello, pull and push over a [`Transport`](spot_sync_http::Transport), under a breaker and a deadline |
//! | [`envelope`] | how a queued bundle travels as one record (shared with the hub half) |
//! | [`IntakeApplier`], [`Intake`] | `NodeIntake` with a persisted serve-prefix cursor; quarantine of what is not ours |
//! | [`OutboxSender`], [`PushSide`] | the node's own outbox as an offer, with the dense push cursor |
//! | [`pg`] | the cursor table, the outbox reader and the cursor stores on the blocking bridge |
//! | [`tls`] | the hub pinned by certificate fingerprint, no system trust store |
//! | [`enrol`] | the one-time code file, redeemed once and wiped; the stored pin |
//! | [`IntakeFactory`] | the seam a host module fills to build the apply path of one tenant |
//! | [`Worker`] | the plain thread the blocking stores live on |
//! | [`StatusHandle`] | `connected to hub-1 (lag 0.2s)` / `offline 3h12m, 418 changes queued` |
//!
//! # Time and randomness are arguments
//!
//! Nothing here reads the wall clock or an ambient random source: the [`Clock`](spot_core::Clock),
//! the [`JitterSource`](spot_sync_http::JitterSource) and the [`Sleeper`](spot_sync_http::Sleeper)
//! are injected, so the loop is deterministic under test.
//!
//! # Offline is normal
//!
//! A hub that is unreachable for hours or days never makes the site not ready (EDG-01): the
//! status says `offline`, the loop backs off, and the site keeps serving reads and writes.
//!
//! # Security
//!
//! The hub is authenticated by its pinned certificate fingerprint ([`tls`]). The sender is
//! untrusted: a served bundle for another tenant or source is quarantined, never applied and
//! never counted into the acknowledged prefix; the prefix comes from settled bundles, never from
//! the hub's claim. No message or log carries a record's content, a key, a code or a
//! certificate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod applier;
pub mod backoff;
pub mod enrol;
pub mod envelope;
pub mod error;
pub mod factory;
pub mod intake;
pub mod link;
pub mod node_loop;
pub mod pg;
pub mod push;
pub mod settings;
pub mod status;
pub mod tls;
pub mod worker;

pub use applier::{CursorStore, Intake, IntakeApplier, Received};
pub use backoff::Backoff;
pub use error::{CODES, Error, Result};
pub use factory::{IntakeFactory, IntakeRequest};
pub use link::HubLink;
pub use node_loop::{LoopParts, NodeLoop};
pub use push::{OutboxEntry, OutboxSender, OutboxSource, PushCursor, PushCursorStore, PushSide};
pub use settings::LoopSettings;
pub use status::{LinkState, StatusHandle, StatusSnapshot};
pub use worker::Worker;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-node-host";
