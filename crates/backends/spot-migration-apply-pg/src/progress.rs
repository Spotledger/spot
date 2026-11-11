//! Live progress over `spot-realtime` (ADR-25).
//!
//! [`RealtimeProgress`] is the engine's [`ProgressSink`] wired to the bus: every event becomes
//! one `migration.progress` notification in the tenant's room
//! `doctype:migration.progress` (the bus scopes a room by [`SiteId`], so the same room name on
//! two sites is two rooms). Per ADR-25 a notification carries **no document data**: the payload
//! is the event's tag, the step index or count and the instant, and a client refetches the
//! authoritative state.
//!
//! # Admin permission
//!
//! Joining the migration room requires an administrator. [`AdminOnly`] is the
//! [`RoomAuthorizer`] that enforces it: a guest is always refused, a principal must be named in
//! the configured admin list, and a room other than the migration room is delegated to an
//! inner authorizer (allow-all by default). The bus calls the authorizer on **every** join,
//! including a resumed subscription, so a permission revoked mid-migration takes effect on the
//! next reconnect.
//!
//! # Never blocks the engine
//!
//! `ProgressSink::on` is synchronous and the bus's `publish` is synchronous and non-blocking,
//! so publishing a progress event never stalls the apply run. A bus that has no subscriber for
//! the room keeps the event in its replay buffer; a slow subscriber is closed as a slow
//! consumer rather than waited for.

use std::sync::Arc;

use serde_json::json;
use spot_core::SiteId;
use spot_migration_apply::progress::{ProgressEvent, ProgressSink};
use spot_realtime::{AllowAll, AuthFuture, Denied, Principal, RealtimeBus, Room, RoomAuthorizer};

use crate::error::Error;

/// The room suffix the migration progress is published to: `doctype:migration.progress`.
///
/// `concat!` keeps the dotted room name out of the error-code scan, which reads plain string
/// literals as stable dotted codes (the same convention `spot-agent-windows`' catalogue uses).
pub const MIGRATION_ROOM: &str = concat!("migration", ".progress");

/// The event kind every notification carries.
pub const PROGRESS_KIND: &str = concat!("migration", ".progress");

/// Publishes the engine's progress events to the tenant's migration room.
pub struct RealtimeProgress {
    bus: RealtimeBus,
    site: SiteId,
    room: Room,
}

impl RealtimeProgress {
    /// A sink publishing to `site`'s migration room on `bus`.
    ///
    /// # Errors
    /// [`Error::ProgressRoom`] when the room name is not a valid room (it is a constant, so
    /// this cannot happen unless the constant changes).
    pub fn new(bus: RealtimeBus, site: SiteId) -> Result<Self, Error> {
        let room = Room::doctype(MIGRATION_ROOM).map_err(|error| Error::ProgressRoom {
            message: error.to_string(),
        })?;
        Ok(Self { bus, site, room })
    }

    /// The room this sink publishes to.
    #[must_use]
    pub fn room(&self) -> &Room {
        &self.room
    }

    /// The site this sink reports for.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// Publishes one event; returns the notification's sequence number in the room.
    ///
    /// # Errors
    /// [`Error::ProgressFailed`] when the event cannot be rendered as a notification payload.
    pub fn publish(&self, event: &ProgressEvent) -> Result<u64, Error> {
        let payload = payload_of(event).map_err(|error| Error::ProgressFailed {
            message: error.to_string(),
        })?;
        Ok(self
            .bus
            .publish(&self.site, &self.room, PROGRESS_KIND, payload))
    }
}

impl ProgressSink for RealtimeProgress {
    fn on(&self, event: ProgressEvent) {
        // A progress notification is best effort by ADR-25: correctness never depends on it,
        // and the engine must not fail a migration because a notification could not be
        // rendered. The engine's journal is the authority; a payload that cannot render is
        // dropped rather than failing the apply.
        let _ = self.publish(&event);
    }
}

impl std::fmt::Debug for RealtimeProgress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RealtimeProgress")
            .field("site", &self.site)
            .field("room", &self.room.as_str())
            .finish_non_exhaustive()
    }
}

/// The notification payload of one event: a tag, the step and count facts, and the instant.
///
/// It never carries a document, a row, a secret or a SQL statement.
fn payload_of(event: &ProgressEvent) -> serde_json::Result<serde_json::Value> {
    let value = match event {
        ProgressEvent::Started { plan_digest, at } => json!({
            "event": "started",
            "plan_digest": plan_digest,
            "at": at,
        }),
        ProgressEvent::GateClosing { reason, at } => json!({
            "event": "gate_closing",
            "reason": reason,
            "at": at,
        }),
        ProgressEvent::GateClosed { at } => json!({ "event": "gate_closed", "at": at }),
        ProgressEvent::HoldBegun { migration_id, at } => json!({
            "event": "hold_begun",
            "migration_id": migration_id,
            "at": at,
        }),
        ProgressEvent::StepStarted { index, id, at } => json!({
            "event": "step_started",
            "step": index,
            "id": id,
            "at": at,
        }),
        ProgressEvent::StepCompleted { index, id, at } => json!({
            "event": "step_completed",
            "step": index,
            "id": id,
            "at": at,
        }),
        ProgressEvent::VerifyStarted { at } => json!({ "event": "verify_started", "at": at }),
        ProgressEvent::VerifyPassed { at } => json!({ "event": "verify_passed", "at": at }),
        ProgressEvent::VerifyFailed {
            mismatches,
            invariants,
            at,
        } => json!({
            "event": "verify_failed",
            "mismatches": mismatches,
            "invariants": invariants,
            "at": at,
        }),
        ProgressEvent::RestoreStarted { cause, at } => json!({
            "event": "restore_started",
            "cause": cause,
            "at": at,
        }),
        ProgressEvent::Restored { mechanism, at } => json!({
            "event": "restored",
            "mechanism": mechanism.label(),
            "at": at,
        }),
        ProgressEvent::HoldReleased { at } => json!({ "event": "hold_released", "at": at }),
        ProgressEvent::HoldDiscarded { reason, at } => json!({
            "event": "hold_discarded",
            "reason": reason,
            "at": at,
        }),
        ProgressEvent::Opened {
            version,
            verified,
            at,
        } => json!({
            "event": "opened",
            "version": match version {
                spot_migration_apply::Version::Old => "old",
                spot_migration_apply::Version::New => "new",
            },
            "verified": verified,
            "at": at,
        }),
        ProgressEvent::Halted { reason, at } => json!({
            "event": "halted",
            "reason": reason,
            "at": at,
        }),
        ProgressEvent::Resumed { from_step, at } => json!({
            "event": "resumed",
            "from_step": from_step,
            "at": at,
        }),
    };
    Ok(value)
}

/// The room authorizer that admits only administrators to the migration room.
///
/// A guest is refused everywhere. A named principal must appear in `admins` to join the
/// migration room; every other room is delegated to `inner` (allow-all unless the host wires
/// the real permission rules).
pub struct AdminOnly {
    admins: Vec<String>,
    inner: Arc<dyn RoomAuthorizer>,
}

impl AdminOnly {
    /// An authorizer admitting the principals in `admins` to the migration room.
    #[must_use]
    pub fn new(admins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            admins: admins.into_iter().map(Into::into).collect(),
            inner: Arc::new(AllowAll),
        }
    }

    /// The same authorizer delegating every other room to `inner`.
    #[must_use]
    pub fn with_inner(mut self, inner: Arc<dyn RoomAuthorizer>) -> Self {
        self.inner = inner;
        self
    }

    /// Whether `principal` is an administrator.
    #[must_use]
    pub fn is_admin(&self, principal: &Principal) -> bool {
        !principal.is_guest() && self.admins.iter().any(|admin| admin == principal.id())
    }

    /// Whether `room` is the migration room.
    #[must_use]
    pub fn is_migration_room(room: &Room) -> bool {
        room.as_str() == format!("doctype:{MIGRATION_ROOM}")
    }
}

impl RoomAuthorizer for AdminOnly {
    fn authorize(&self, site: &SiteId, principal: &Principal, room: &Room) -> AuthFuture<'_> {
        if principal.is_guest() {
            return Box::pin(async {
                Err(Denied::new(
                    "a guest may not subscribe to tenant notifications",
                ))
            });
        }
        if Self::is_migration_room(room) && !self.is_admin(principal) {
            return Box::pin(async {
                Err(Denied::new(
                    "migration progress is visible to administrators only",
                ))
            });
        }
        if Self::is_migration_room(room) {
            return Box::pin(async { Ok(()) });
        }
        self.inner.authorize(site, principal, room)
    }
}

impl std::fmt::Debug for AdminOnly {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdminOnly")
            .field("admins", &self.admins)
            .finish_non_exhaustive()
    }
}
