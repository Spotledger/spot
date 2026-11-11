//! Result and error types for joining, receiving and authorisation.

use thiserror::Error;

/// A permission denial handed back by a [`RoomAuthorizer`](crate::RoomAuthorizer).
///
/// Carries a human-readable reason only: realtime is notification, not data sync (ADR-25),
/// and the client is expected to refetch — the denial must not leak room contents.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("join denied: {reason}")]
pub struct Denied {
    /// Why the join was refused, as reported by the authorizer.
    reason: String,
}

impl Denied {
    /// A denial with the given reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The denial reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// The outcome of a successful [`Subscription::join`](crate::Subscription::join).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinOutcome {
    /// Joined; nothing needed replaying.
    Joined,
    /// Joined and `count` buffered events after the resume point were delivered.
    Replayed {
        /// How many events were replayed into the subscription's queue.
        count: usize,
    },
    /// Joined, but the events after `resume_after` cannot be replayed: they have left the
    /// replay buffer (too old or too many missed), or they are from before this
    /// incarnation of the `(site, room)` — a recreated room's counter is seeded at the
    /// bus's high-water mark, so its first handed-out seq is strictly above anything an
    /// earlier incarnation handed out and resume points from before a reset can never
    /// pass as gap-free. A `resume_after` *beyond* the room's latest `seq` is the same
    /// reset,
    /// detected from the other side: a room's counter never goes backwards. The client
    /// must refetch from the API before relying on realtime events; the room is still
    /// joined, and no gap in the live stream results — older events are never sent.
    ResyncRequired,
}

/// Why a [`Subscription::join`](crate::Subscription::join) failed.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum JoinError {
    /// The [`RoomAuthorizer`](crate::RoomAuthorizer) refused the join. The subscription did
    /// not enter the room and will receive nothing from it.
    #[error("join denied: {0}")]
    Denied(Denied),
    /// The subscription is already closed (for example as a slow consumer); it can no
    /// longer receive events. A resumed join also yields this — instead of a half-joined
    /// `Ok` — when the replay for `resume_after` does not fit the subscription's queue:
    /// the subscription is closed as a slow consumer right then.
    #[error("subscription is closed")]
    Closed,
}

/// Why a [`Subscription::recv`](crate::Subscription::recv) ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum Closed {
    /// The subscription's queue filled up: the bus does not wait for slow clients
    /// (ADR-25), it disconnects them. Remaining buffered events were discarded; reconnect
    /// with a resume point to get a fresh subscription.
    #[error("subscription closed: slow consumer")]
    SlowConsumer,
    /// The channel ended for another reason: the subscription or the bus was dropped.
    #[error("subscription closed")]
    ChannelClosed,
}
