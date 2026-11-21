//! PostgreSQL stores for link exchange (WP-CTL-10.T3; ADR-058 rules 8, 9, 13 and 14; ADR-080).
//!
//! `spot-link-exchange` (WP-CTL-10.T1) is the exchange core: signed messages, the sender
//! [`Outbox`](spot_link_exchange::Outbox) with its delivery states and the receiver
//! [`Inbox`](spot_link_exchange::Inbox) with idempotent, ordered apply and parking. This crate
//! makes its three ports durable, in each tenant's own schema:
//!
//! * [`PgOutbox`]: the [`OutboxStore`](spot_link_exchange::OutboxStore) (messages, states, slot
//!   fillers, producer events);
//! * [`PgInbox`]: the [`InboxStore`](spot_link_exchange::InboxStore) (conversations, entries,
//!   parking, the dedupe by message id);
//! * [`PgNoticeFeed`]: the [`NoticeFeed`](spot_link_exchange::NoticeFeed) consumers read;
//! * [`RetentionSweep`]: the job that expires, refuses and purges.
//!
//! # The notice is part of the inbox transaction
//!
//! [`PgInbox::commit`](spot_link_exchange::InboxStore::commit) writes the entries, the parking
//! changes, the rejections, the conversation cursor **and the delivery notices** in one
//! transaction ([`inbox::apply_commit`]). The notice cannot exist without its message and the
//! message cannot be applied without its notice; there is no other function that appends a
//! notice. See [`inbox`] for how the kill tests prove it.
//!
//! # Cursors
//!
//! Notice cursors and producer-event positions are assigned from a counter row that the
//! appending transaction holds locked until it commits, so they are gapless and in commit order
//! and a bookmark can never skip a late commit (ADR-053's safe-bookmark rule; see [`schema`]).
//!
//! # Time, secrets and SQL
//!
//! The crate reads no clock (every instant is an argument) and uses no floats or `unsafe`. Every
//! SQL statement is static text with bound parameters; failures carry fixed sentences, never a
//! driver message, a connection string or a value. Payloads are business data and are stored
//! only in the entry documents; no log line contains one.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, like the other `*-pg` crates; with `SPOT_REQUIRE_PG=1` a missing
//! server is a failure.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod blocking;
mod codec;
pub mod error;
pub mod feed;
pub mod inbox;
pub mod outbox;
pub mod retention;
pub mod schema;
mod scope;

pub use error::MIGRATION_FAILED;
pub use feed::PgNoticeFeed;
pub use inbox::{PgInbox, apply_commit};
pub use outbox::PgOutbox;
pub use retention::{RetentionSweep, SWEEP_JOB, SweepConfig, SweepHost, SweepReport};
pub use schema::{TENANT_DDL, TENANT_TABLES};
pub use scope::PgExchange;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-link-exchange-pg";
