//! The hub's acknowledgement ledger (ADR-065 sections 1, 3 and 8).
//!
//! An acknowledgement means *applied*. The obligation ledger is **derived**, never stored per
//! record: [`AckLedger`] keeps, per `(node, tenant)`, the applied prefix, the last contact time
//! (hub time) and the software version, and asks `spot-sync-obligations` which of that tenant's
//! registered mandatory events are still outstanding. [`AckLedger::tick`] turns the clock into
//! the escalation ladder: each level change yields the notifications to send and raises or
//! clears conditions (`OBLIGATION_DUE_SOON`, `OBLIGATION_OVERDUE`, `OBLIGATION_RED` on subject
//! `node:<id>@tenant:<id>`) through the conditions framework, one audit event per transition
//! into the caller's `AuditSink`, carrying the node, the tenant, the event positions and the
//! level as parameters and no payload. [`AckLedger::who_is_missing`] and
//! [`AckLedger::evidence`] are the hub's *as-of* view: every row says when the node last
//! reported.
//!
//! # Tenancy
//!
//! Every tenant has its own journal and its own position space (ADR-061), so a position or a
//! prefix means nothing without its tenant: tenant A's position 40 and tenant B's position 40
//! are different events. The unit of state is therefore `(node, tenant)`, and the tenant is a
//! plain argument of every public method that reads or writes tenant state: there is no default
//! tenant, no `Option`, and no tenant-less overload. The only method without a tenant is
//! [`AckLedger::set_version`], which is a statement about the node and applies to every record
//! of that node; a check-in names its tenant and touches only that `(node, tenant)` record. An ack for a pair that was not registered is
//! [`AckError::UnknownNode`]; it never falls back to another tenant's record, so one tenant's
//! ack can never satisfy another tenant's event, move its level or clear its condition.
//!
//! The tenant id is **hub data** (from the authenticated session's tenant binding), the node id
//! likewise; the prefix and the holds are **node data**, bounded by the head of that tenant's
//! own stream. Ids containing `@` are refused at registration, so a condition subject names
//! exactly one pair.
//!
//! # Trust and time
//!
//! **The transport must pass the authenticated node id and tenant.** This crate takes the [`str`]
//! ids as given: it never learns who a caller is, so an ack for node A can never be made to
//! clear node B's obligation, but only if the caller maps the connection's credentials to the
//! ids it passes.
//! Acks for a node that was never registered are refused.
//!
//! **The head is hub data; the prefix is node data.** Every node-supplied position ([`AckLedger::on_ack`]'s
//! prefix and the positions of [`AckLedger::set_holds`]) is bounded by a [`StreamHead`], the
//! newest position the hub's own stream holds, passed by the caller from the hub and never
//! derived from anything a node said. The invariant: *an ack never covers a position the hub had
//! not written when the ack arrived*. A hostile or corrupt `u64::MAX` is refused with nothing
//! changed, so a mandatory event registered afterwards at a higher position is still outstanding.
//!
//! Nothing here reads a clock or a random source: `at` and `now` are arguments in **hub time**,
//! milliseconds since the Unix epoch, and must not go backwards between calls. A node's own
//! clock is never used.
//!
//! Out of scope: transport, notification delivery, the Admin Center list.

mod defs;
mod error;
mod head;
mod ledger;
mod rows;
mod store;

pub use defs::{OBLIGATION_DUE_SOON, OBLIGATION_OVERDUE, OBLIGATION_RED};
pub use error::{
    AckError, CONDITION_MISSING, HOLD_BEYOND_HEAD, HOLD_UNKNOWN, ID_INVALID, PREFIX_BEYOND_HEAD,
    PREFIX_REGRESSED, UNKNOWN_EVENT, UNKNOWN_NODE,
};
pub use head::StreamHead;
pub use ledger::{AckLedger, AckOutcome, DueAction, LevelChange, TenantNotify, TickReport};
pub use rows::NodeReportRow;
pub use store::{MemoryStore, NodeRecord, NodeStateStore};
