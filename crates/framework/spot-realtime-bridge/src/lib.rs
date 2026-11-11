//! `spot-realtime-bridge`: the feed and the permission rules of the realtime subsystem
//! (blueprint WS3, WP-PLT-08.T3, ADR-25, contract `contracts/native-api/realtime.md`).
//!
//! `spot-realtime` (WP-PLT-08.T1/T2) provides the bus and the WebSocket transport; nothing
//! fed them, and every join ran through [`spot_realtime::AllowAll`]. This crate supplies
//! the two missing pieces, and nothing else:
//!
//! - [`pump::FeedPump`]: reads one site's [`ChangeFeed`](spot_storage_contract::ChangeFeed)
//!   — the change feed and the outbox — from a durable position and publishes room events
//!   ([`Room`](spot_realtime::Room)s `doc:<doctype>:<id>`, `doctype:<doctype>`,
//!   `user:<id>`) onto the [`RealtimeBus`](spot_realtime::RealtimeBus) of that site. It is
//!   a pure [`FeedPump::pump_once`] the host loop calls; no spawned tasks, no timers.
//! - [`auth::PermissionRoomAuthorizer`]: a [`RoomAuthorizer`](spot_realtime::RoomAuthorizer)
//!   over the permission core (`spot-permissions`). `site` for any signed-in non-guest;
//!   `user:<id>` only for that user; `doctype:<dt>` for anyone with read on the DocType;
//!   `doc:<dt>:<id>` for anyone the engine's `can(Read)` admits on the document, with the
//!   document facts loaded through the injected [`auth::DocFactsSource`].
//!
//! # Notification, not data sync (ADR-25)
//!
//! Every event carries identifiers and a version and **at most field names** — never a
//! field value. The one exception is a `sys_user_notice` (what `HookContext::notify_user`
//! writes): its `data` is app-supplied JSON, so the pump guards its **shape** — an object of
//! ids and short scalars, at most 2 KiB — and never its content, which the caller owns (see
//! [`pump::TOPIC_NOTICE`]). The pump derives its events from committed entries only (the
//! change feed and the outbox hold nothing else), so nothing is ever published before its
//! transaction commits. A stalled subscriber is closed by the bus (`Closed::SlowConsumer`,
//! wire code 4008) and never delays another one; `publish` is synchronous and never awaits
//! anyone. No correctness may depend on a realtime message arriving: clients refetch.
//!
//! # Admission is decided at join; `revalidate` is the compensating control
//!
//! Delivery follows the same rule authorisation does — **admission is decided at join**
//! (ADR-25, contract: "Permissions are checked on join", re-checked at most within the
//! permission cache's lifetime). A document event reaches a `doctype:` room only through
//! subscribers who proved list read on the DocType when they joined, and a `doc:` room
//! only through subscribers who proved `can(Read)` on that document. Between joins nothing
//! re-checks anything: no per-event re-authorisation happens, by design — the per-event
//! check would have to run against one snapshot per subscriber per publish and re-open
//! every ADR-25 guarantee the bus already made.
//!
//! The compensating control for a permission that changes while a principal is joined is
//! [`auth::PermissionRoomAuthorizer::revalidate`]: the host calls it when the permission
//! snapshot is swapped, with the rooms the principal is joined to, and leaves every room
//! the principal can no longer read (a `user:` room is never evictable — it is not a
//! permission question). The actual `leave` runs where the subscription lives (the
//! WebSocket session layer of the host binary), because `spot-realtime` keeps membership
//! inside its subscriptions; this trait method is the decision, the session executes it.
//!
//! # Event identity and redelivery
//!
//! Every published event's payload carries its `event_id`: a change-feed entry's id is
//! derived deterministically from its feed position (so a re-read after a crash —
//! at-least-once delivery — re-publishes the same id), and an outbox event carries its own
//! record id. Clients deduplicate on it and refetch what they show.

pub mod auth;
pub mod error;
pub mod pump;

pub use auth::{
    DENY_MESSAGE, DocFactsSource, EngineSource, PermissionRoomAuthorizer, PrincipalSource,
};
pub use error::{BridgeError, CODES};
pub use pump::{
    FedRooms, FeedHead, FeedPump, MappingOutcome, NOTICE_EVENT, NOTICE_MAX_BYTES,
    NOTICE_MAX_TEXT_BYTES, PositionStore, PumpConfig, PumpOutcome, PumpPosition, PumpTelemetry,
    RESYNC_KIND, TOPIC_NOTICE,
};
