//! `spot-bpm-conversations-pg` — the PostgreSQL
//! [`ConversationStore`](spot_bpm_conversations::ConversationStore) for conversations between
//! organisations (WP-BPM-12.T6, ADR-080, ADR-49).
//!
//! `spot-bpm-conversations` ships only the in-memory store, so a conversation loses its state on
//! restart. This crate keeps it: [`PgConversationStore`] implements the trait over
//! `spot_bpm_conversation` and `spot_bpm_conversation_rejection` (see [`schema`]), created by
//! [`PgConversationStore::prepare`].
//!
//! # The sync trait and the bridge
//!
//! `ConversationStore` is synchronous and PostgreSQL is not. As in `spot-bpm-tasks-pg`
//! (WP-BPM-05.T6), the bridge is the shared, bounded `spot-pg-bridge` (ADR-082 addendum 1): the
//! caller's thread blocks on a channel while the statement runs on the bridge's own runtime,
//! with a deadline, a bound on work in flight and a statement cancel on timeout. This crate
//! writes no `block_on` of its own.
//!
//! **Fail closed, never deadlock.** A call from a thread that belongs to any tokio runtime is
//! refused with `bpm_conversations.store_failed`; nothing is run. A host drives the conversation
//! service from a plain thread of its control or job process.
//!
//! # What the store keeps
//!
//! * One row per conversation, keyed `(tenant, link, conversation)`. `create` is
//!   `INSERT .. ON CONFLICT DO NOTHING`: an existing key is `Conflict` and nothing is written.
//! * `save` is one `UPDATE .. WHERE version = $expected`: a stale version updates nothing
//!   (`Conflict`), and of two racing saves exactly one wins.
//! * The whole record is `body` (jsonb, at most 1 MiB; a larger record is refused with
//!   `TooLong` before a statement runs, and a `CHECK` holds the line below the store).
//! * A bounded, per-link memory of rejections, trimmed in the same transaction that adds one.
//!
//! # Security
//!
//! * **Tenant isolation.** The tenant is a constructor argument; every statement filters on it
//!   as parameter `$1`, and no method takes one. Both tables carry it in their primary key.
//! * **No injection.** All statements are fixed text in [`sql`] with bound parameters.
//! * **Fail closed.** A backend failure is `Store`, never an empty result; a row that disagrees
//!   with its own columns or cannot be decoded is an error, not skipped.
//! * **Quiet errors.** `ConversationError::Store` carries no text, so no row content, payload,
//!   key or SQL can reach a message; this crate logs nothing.
//! * **No partial write.** A refused write (stale version, duplicate key, oversized record,
//!   backend failure) leaves no changed or half-written row; the caller records the refusal.
//!   The record keeps its own audit facts: the version, and the time (`at_ms`) and direction of
//!   every message.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, exactly as the other `*-pg` crates do; with
//! `SPOT_REQUIRE_PG=1` the helper fails instead of skipping.

#![forbid(unsafe_code)]

mod codec;
pub mod error;
pub mod schema;
pub mod sql;
mod store;

pub use error::{BAD_SETUP, MIGRATION_FAILED, SetupError};
pub use schema::MAX_BODY_BYTES;
pub use store::{PREPARE_DEADLINE, PgConversationStore, READ_DEADLINE, WRITE_DEADLINE};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-conversations-pg";
