//! In-process realtime event bus (blueprint WS3, WP-PLT-08, ADR-25).
//!
//! `spot-realtime` delivers **notifications, not data** (ADR-25): an [`Event`] carries a room,
//! a per-room sequence number, a small `kind`/payload hint — never document contents. Clients
//! refetch from the API on reconnect; no correctness may depend on a message arriving.
//!
//! The core pieces:
//! - [`Room`]: a validated, structured room name (`doc:…`, `doctype:…`, `user:…`, `site`),
//!   scoped to a [`spot_core::SiteId`] — the same room name on two sites is two rooms.
//! - [`RealtimeBus`]: publish/subscribe with bounded, non-blocking delivery. `publish` is
//!   synchronous and never awaits a subscriber; a subscriber whose queue overflows is closed
//!   ([`Closed::SlowConsumer`]) instead of being waited for.
//! - [`RoomAuthorizer`]: the permission seam. Every join — including joins made while
//!   resuming a subscription — is authorised. The rules themselves live in WP-PLT-03.
//! - [`PendingEvents`]: the after-commit buffer the transaction layer fills during a
//!   transaction and [`PendingEvents::flush`]es only once the commit succeeded; dropping it
//!   without flushing is the rollback path.
//!
//! ## Where the later layers plug in
//!
//! - **WebSocket protocol** (feature `ws`, on by default): [`ws`] is the native transport of
//!   `contracts/native-api/realtime.md` (WP-PLT-08.T2, ADR-055) — `ws::router` is the single
//!   `GET /api/v1/realtime` route mounted by the server's site routes; a transport task owns
//!   one [`Subscription`], awaits [`Subscription::recv`] and writes frames; joins call
//!   [`Subscription::join`] with the client's `resume_after`. Its [`ws::TokenVerifier`] is
//!   the seam that answers `auth`; the permission rules still come from
//!   [`RoomAuthorizer`].
//! - **Cross-node fan-out**: `publish` is the single funnel for new events. A future inbound
//!   invalidation-bus consumer feeds remote events through the same publish path for the
//!   `(site, room)` this node serves, so replay, sequencing and slow-consumer handling apply
//!   uniformly. `Room::parse` exists so a room name arriving over the wire is re-validated
//!   before use.
//!
//! ## Room seqs never go backwards
//!
//! A room is freed once its last subscriber leaves and its replay buffer has aged out.
//! Its `seq` must still never be reused: the bus tracks one high-water mark — the highest
//! `seq` it has ever handed out — and seeds every new room's counter from it, so the
//! room's first handed-out seq (and every later one) sits strictly above whatever every
//! earlier incarnation handed out; the fresh room's `lost_before` watermark is that first
//! handed-out seq, which for the bus's very first room means seqs count from 1. Handing
//! out a seq moves the mark, and the mark never wraps: a bus that exhausts `u64::MAX`
//! seqs panics instead of reusing one. So any `resume_after` point from an earlier
//! incarnation of the `(site, room)` — however the room was reset — falls strictly below
//! `lost_before` or beyond the room's counter, and yields `ResyncRequired`, never a
//! silent gap: `Joined` or `Replayed` always means the client missed nothing that was
//! replayable.
//!
//! ## Housekeeping
//!
//! The hot paths stay local: `publish` costs one clone plus one `try_send` per
//! subscriber of the published room, and ages only that room; `leave`, subscription drop
//! and close free only the rooms *those calls* emptied. Full global ageing — every room
//! pruned, every emptied room freed — runs on the cold entry point
//! [`RealtimeBus::stats`]. `publish` additionally runs a bounded amortised sweep: every
//! [`SWEEP_INTERVAL_PUBLISHES`] publishes, one sweep visits at most
//! [`SWEEP_SCAN_BUDGET`] rooms from a rotating window of the room list — modelled on
//! `spot-cache`'s `SWEEP_INTERVAL_WRITES` / `SWEEP_SCAN_BUDGET` — so an idle room is
//! aged out and freed by ordinary publish traffic alone without any publish ever
//! scanning the whole map. Memory stays bounded either way: each room's replay buffer
//! never exceeds `replay_len` events.
//!
//! ## Concurrency rule
//!
//! No `std::sync` lock is ever held across an `.await`: authorisation happens before the bus
//! state lock is taken, and delivery into each subscriber's bounded `tokio::sync::mpsc` queue
//! is a non-blocking `try_send`. Locks are nested in one order only — the bus state
//! ([`RealtimeBus`]'s guarded maps) first, a subscription's room-membership set second;
//! the membership set is otherwise used on its own, never held while the state lock is
//! taken.

mod auth;
mod bus;
mod error;
mod event;
mod pending;
mod room;
#[cfg(feature = "ws")]
pub mod ws;

pub use auth::{AllowAll, AuthFuture, RoomAuthorizer};
pub use bus::{
    BusConfig, BusStats, RealtimeBus, SWEEP_INTERVAL_PUBLISHES, SWEEP_SCAN_BUDGET, Subscription,
};
pub use error::{Closed, Denied, JoinError, JoinOutcome};
pub use event::{Event, Principal};
pub use pending::PendingEvents;
pub use room::{InvalidRoom, Room};
