//! The fault vocabulary: what the proxy can do to a connection, when, and the timeline it
//! records.
//!
//! A fault is scheduled as a [`ScheduledFault`]: which connection, which direction, what
//! triggers it, and which [`Fault`] it applies. The plan is built by the test, usually from a
//! seed, and every fault that fires is appended to a [`FaultTimeline`] with the time it fired,
//! so a failure can be replayed from its seed and its timeline.
//!
//! The names match the task brief: `pass`, `delay(ms)`, `drop_after_bytes(n)`, `reset_now`,
//! `blackhole`, `half_close`, `reset_when_message(type)`. See [`Fault`] and [`Trigger`].

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::wire::PgMessage;

/// Which way the bytes are going.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// From the backend to PostgreSQL.
    ClientToServer,
    /// From PostgreSQL back to the backend.
    ServerToClient,
}

impl Direction {
    /// The name used in reports.
    pub fn name(self) -> &'static str {
        match self {
            Direction::ClientToServer => "c2s",
            Direction::ServerToClient => "s2c",
        }
    }
}

/// Which connection a scheduled fault applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnSelector {
    /// Every connection through the proxy.
    Any,
    /// The connection with this 1-based ordinal.
    Ordinal(u64),
}

/// When a fault fires within one direction of one connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// As soon as the connection is established.
    Immediate,
    /// Once this many bytes have been relayed in the direction.
    AfterBytes(u64),
    /// On a message of a given type.
    OnMessage(MessageTrigger),
}

/// A message trigger: which message, whether the triggering message is relayed first, how many
/// occurrences to skip, and whether it fires again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTrigger {
    /// The message type to watch for.
    pub message: PgMessage,
    /// `true` to relay the triggering message before acting, `false` to cut it off.
    pub after: bool,
    /// Occurrences to skip before the first fire.
    pub skip: u32,
    /// `true` to fire on every later occurrence, `false` to fire once.
    pub repeat: bool,
}

impl MessageTrigger {
    /// A one-shot trigger on the next `message`.
    pub fn once(message: PgMessage, after: bool) -> MessageTrigger {
        MessageTrigger {
            message,
            after,
            skip: 0,
            repeat: false,
        }
    }

    /// A trigger that fires on every occurrence of `message` after `skip` of them.
    pub fn every(message: PgMessage, after: bool, skip: u32) -> MessageTrigger {
        MessageTrigger {
            message,
            after,
            skip,
            repeat: true,
        }
    }

    /// Whether the trigger fires for `message`, given the connection's running occurrence count.
    pub fn observe(&self, message: PgMessage, occurrences: &mut u32) -> bool {
        if message != self.message {
            return false;
        }
        *occurrences += 1;
        if *occurrences <= self.skip {
            return false;
        }
        self.repeat || *occurrences == self.skip + 1
    }
}

/// What a fault does to the connection once it fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fault", rename_all = "snake_case")]
pub enum Fault {
    /// Relay normally.
    Pass,
    /// Add this latency to every relayed chunk in the direction.
    Delay {
        /// Milliseconds added.
        ms: u64,
    },
    /// Stop relaying and close the connection.
    Drop,
    /// Close the connection at once (no further bytes relayed).
    Reset,
    /// Swallow traffic for this long, then relay again.
    Blackhole {
        /// Milliseconds the connection is silent.
        ms: u64,
    },
    /// Stop relaying and shut down only the write half.
    HalfClose,
}

impl Fault {
    /// The name used in reports.
    pub fn name(self) -> &'static str {
        match self {
            Fault::Pass => "pass",
            Fault::Delay { .. } => "delay",
            Fault::Drop => "drop_after_bytes",
            Fault::Reset => "reset",
            Fault::Blackhole { .. } => "blackhole",
            Fault::HalfClose => "half_close",
        }
    }

    /// The convenience constructor `delay(ms)`.
    pub fn delay(ms: u64) -> Fault {
        Fault::Delay { ms }
    }

    /// The convenience constructor `blackhole(ms)`.
    pub fn blackhole(ms: u64) -> Fault {
        Fault::Blackhole { ms }
    }
}

/// One scheduled fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledFault {
    /// Which connection it applies to.
    pub conn: ConnSelector,
    /// Which direction it watches.
    pub direction: Direction,
    /// What fires it.
    pub trigger: Trigger,
    /// What it does.
    pub fault: Fault,
    /// Stop after this many fires (`None`: never stop).
    pub max_fires: Option<u32>,
}

impl ScheduledFault {
    /// A fault on the first connection, in `direction`.
    pub fn new(direction: Direction, trigger: Trigger, fault: Fault) -> ScheduledFault {
        ScheduledFault {
            conn: ConnSelector::Any,
            direction,
            trigger,
            fault,
            max_fires: None,
        }
    }

    /// The same fault, limited to one connection.
    pub fn on_connection(mut self, conn: ConnSelector) -> ScheduledFault {
        self.conn = conn;
        self
    }

    /// The same fault, firing at most `n` times.
    pub fn at_most(mut self, n: u32) -> ScheduledFault {
        self.max_fires = Some(n);
        self
    }

    /// `reset_when_message(type)`: cut the connection on a message of this type, relaying it
    /// first when `after` is true.
    pub fn reset_when_message(
        direction: Direction,
        message: PgMessage,
        after: bool,
    ) -> ScheduledFault {
        ScheduledFault::new(
            direction,
            Trigger::OnMessage(MessageTrigger::once(message, after)),
            Fault::Reset,
        )
    }

    /// `drop_after_bytes(n)`: close once `n` bytes have been relayed.
    pub fn drop_after_bytes(direction: Direction, n: u64) -> ScheduledFault {
        ScheduledFault::new(direction, Trigger::AfterBytes(n), Fault::Drop)
    }
}

/// One fault that fired, with when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultEvent {
    /// Microseconds since the proxy started.
    pub at_us: u64,
    /// The connection's 1-based ordinal.
    pub conn: u64,
    /// The direction.
    pub direction: Direction,
    /// The fault.
    pub fault: Fault,
    /// A short, secret-free note (for example the message that triggered it).
    pub detail: String,
}

/// The faults that fired, in order, with times.
#[derive(Debug)]
pub struct FaultTimeline {
    started: Instant,
    events: Vec<FaultEvent>,
}

impl Default for FaultTimeline {
    fn default() -> Self {
        FaultTimeline::new()
    }
}

impl FaultTimeline {
    /// An empty timeline.
    pub fn new() -> FaultTimeline {
        FaultTimeline {
            started: Instant::now(),
            events: Vec::new(),
        }
    }

    /// Records one fired fault, timestamped against the timeline's start.
    pub fn record(
        &mut self,
        conn: u64,
        direction: Direction,
        fault: Fault,
        detail: impl Into<String>,
    ) {
        let at_us = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.events.push(FaultEvent {
            at_us,
            conn,
            direction,
            fault,
            detail: detail.into(),
        });
    }

    /// The events recorded so far.
    pub fn events(&self) -> &[FaultEvent] {
        &self.events
    }

    /// Whether nothing has fired.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// A compact, secret-free one-line-per-event rendering.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for e in &self.events {
            out.push_str(&format!(
                "{:>10}us conn={} dir={} fault={} {}\n",
                e.at_us,
                e.conn,
                e.direction.name(),
                e.fault.name(),
                e.detail
            ));
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_once_trigger_fires_exactly_once() {
        let t = MessageTrigger::once(PgMessage::Sync, false);
        let mut n = 0;
        assert!(t.observe(PgMessage::Sync, &mut n));
        assert!(!t.observe(PgMessage::Sync, &mut n));
        assert!(!t.observe(PgMessage::Execute, &mut n));
    }

    #[test]
    fn a_repeating_trigger_skips_then_fires_every_time() {
        let t = MessageTrigger::every(PgMessage::ReadyForQuery, true, 2);
        let mut n = 0;
        assert!(!t.observe(PgMessage::ReadyForQuery, &mut n));
        assert!(!t.observe(PgMessage::ReadyForQuery, &mut n));
        assert!(t.observe(PgMessage::ReadyForQuery, &mut n));
        assert!(t.observe(PgMessage::ReadyForQuery, &mut n));
    }

    #[test]
    fn the_named_constructors_build_the_briefs_faults() {
        let r = ScheduledFault::reset_when_message(
            Direction::ServerToClient,
            PgMessage::ReadyForQuery,
            true,
        );
        assert_eq!(r.fault, Fault::Reset);
        assert_eq!(
            r.trigger,
            Trigger::OnMessage(MessageTrigger::once(PgMessage::ReadyForQuery, true))
        );
        let d = ScheduledFault::drop_after_bytes(Direction::ClientToServer, 64);
        assert_eq!(d.fault, Fault::Drop);
        assert_eq!(d.trigger, Trigger::AfterBytes(64));
        assert_eq!(Fault::delay(500).name(), "delay");
        assert_eq!(Fault::blackhole(5_000).name(), "blackhole");
    }

    #[test]
    fn the_timeline_records_in_order() {
        let mut t = FaultTimeline::new();
        assert!(t.is_empty());
        t.record(1, Direction::ClientToServer, Fault::Reset, "sync");
        t.record(1, Direction::ServerToClient, Fault::Delay { ms: 500 }, "");
        assert_eq!(t.events().len(), 2);
        assert_eq!(t.events()[0].conn, 1);
        assert!(t.events()[0].at_us <= t.events()[1].at_us);
        assert!(t.to_text().contains("reset"));
    }
}
