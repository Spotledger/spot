//! The hub's durable state on PostgreSQL (WP-RTB-02.T1, ADR-062 sections 2 to 4, ADR-082 class C).
//!
//! `spot-sync-hub`'s relay works over traits whose only implementations were in memory. This crate
//! keeps what a hub must not forget, per tenant and transactionally:
//!
//! | Type | Implements | Holds |
//! | --- | --- | --- |
//! | [`PgHubStream`] | `HubStream` | reads the tenant outbox (`spot_outbox` and its published positions) in order, whole source transactions only, through an [`EntryMapper`] |
//! | [`PgSendQueue`] | `SendQueue` | the per-destination queue with serve sequences, bounded in rows and bytes (`sync_hub_pg.queue_full`, never a silent drop) |
//! | [`PgRelayStore`] | (state) | the relay's cursor and each destination's holds, extras and backlog (`RelayState`/`DestSnapshot` as JSON) |
//! | [`PgNodeRegistry`] | (registry) | nodes, lifecycle state and subscriptions; an invalid subscription makes the node receive nothing |
//! | [`PgAckBook`] | `AckObserver` | the acknowledgement book: [`PgAckBook::ack_and_trim`] records an acknowledgement and trims the queue in **one transaction** |
//! | [`PgNodeStateStore`] | `NodeStateStore` | the obligation ledger's node records and mandatory events |
//!
//! # Where this store may be used (ADR-082 section 2)
//!
//! The traits are synchronous, so every call crosses the shared blocking bridge
//! ([`spot_pg_bridge::Bridge`], one per process). That is legitimate in the hub's control-plane
//! process, never on a request handler or any task running on an async runtime: such a call is
//! refused (`pg_bridge.wrong_thread`) and building a store there is refused with
//! `sync_hub_pg.unsupported_context`. Every call has a stated deadline. A timed-out write may or
//! may not have been applied; every write here is idempotent or safe to inspect and retry.
//!
//! # Tenancy
//!
//! Every hub table's key starts with the tenant, and **every statement** on them carries the
//! tenant predicate: [`STATEMENTS`] lists them and a test proves each one has it. The tenant is the
//! store's configuration, never a value a node sent. Two tenants can use the same node ids and
//! destination names without any read, trim, acknowledgement or count crossing.
//!
//! # Failure
//!
//! Failures carry fixed sentences and the bridge's code, never SQL, a payload or a row value. A
//! trait that cannot return an error (`NodeStateStore`, `AckObserver`) records the failure for
//! `take_failure` and answers in the safe direction (an unreadable node is unknown).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod ack;
mod db;
pub mod error;
mod images;
mod nodes;
mod queue;
mod relay_state;
pub mod schema;
mod stream;

pub use ack::{AckTrim, PgAckBook, PgNodeStateStore};
pub use db::{Db, PREPARE_DEADLINE, PgHub};
pub use error::PgHubError;
pub use images::{IMAGE_DEADLINE, PgOutboxImages, current_generation};
pub use nodes::{NodeRow, NodeState, PgNodeRegistry};
pub use queue::{PgSendQueue, QueueLimits, is_queue_full};
pub use relay_state::PgRelayStore;
pub use stream::{EntryMapper, OutboxRow, PgHubStream};

/// Every statement this crate runs on a hub table, so a test can prove none lacks the tenant
/// predicate. DDL and the read of the tenant's own outbox (which lives in the tenant's own
/// database and has no tenant column) are not listed.
pub const STATEMENTS: &[&str] = &[
    queue::PUSH_LOCK,
    queue::PUSH_ROW,
    queue::PUSH_EXISTS,
    queue::PUSH_BUMP,
    queue::PUSH_COUNT,
    queue::PUSH_INSERT,
    queue::PEEK,
    queue::ACK_POSITION,
    queue::LAST_SEQ,
    queue::LOWEST_AFTER,
    queue::ACK_SERVED,
    queue::REMOVE_REDELIVERED,
    queue::BYTES,
    queue::DEPTH,
    queue::OLDEST_AGE,
    relay_state::LOAD_RELAY,
    relay_state::SAVE_RELAY,
    relay_state::LOAD_DESTS,
    relay_state::SAVE_DEST,
    relay_state::LIST_SNAPSHOTS,
    relay_state::DROP_DEST,
    nodes::UPSERT,
    nodes::GET,
    nodes::LIST,
    nodes::SET_STATE,
    nodes::ACTIVE,
    ack::NODE_ACK,
    ack::NODE_LAST,
    ack::NODE_PREFIX,
    ack::LOAD_RECORD,
    ack::SAVE_RECORD,
    ack::LIST_RECORDS,
    ack::LIST_EVENTS,
    ack::SAVE_EVENT,
];

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-hub-pg";
