//! What the shipper reports: the durable position other services wait on (the acknowledgement
//! gate of WP-RTB-01.T4), the lag, and the readiness text.

use std::time::Duration;

use spot_events_contract::envelope::Position;
use tokio::sync::watch;

/// The shipper's state as readers see it. Cheap to copy; published after every step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// The last position made durable in the journal: `(0, 0)` before anything was shipped.
    pub durable: Position,
    /// Entries read from the outbox in the last step that are not yet durable (0 when caught up;
    /// a full page means "at least this many").
    pub behind_entries: u64,
    /// How long, in seconds, some entry has been waiting to become durable (0 when none).
    pub lag_secs: u64,
    /// Whether this node holds the writer lease.
    pub writer: bool,
    /// The step failed and is being retried (store unreachable, protect failure, watermark).
    pub degraded: bool,
    /// For how long the shipper has been failing, in seconds.
    pub degraded_secs: u64,
    /// The code the shipper stopped with (a broken chain needs a person), if it has.
    pub stopped: Option<&'static str>,
    /// When the last segment was sealed (unix milliseconds).
    pub last_seal_millis: Option<i64>,
    /// Entries made durable since the shipper started.
    pub shipped_total: u64,
}

impl Progress {
    /// Before the first step.
    #[must_use]
    pub fn initial() -> Self {
        Self {
            durable: Position {
                generation: 0,
                seq: 0,
            },
            behind_entries: 0,
            lag_secs: 0,
            writer: false,
            degraded: false,
            degraded_secs: 0,
            stopped: None,
            last_seal_millis: None,
            shipped_total: 0,
        }
    }

    /// The readiness text: `journal: remote-sync ok (lag 0s, shipped g1:s20931)`,
    /// `journal: degraded (store unreachable for 41s, RPO risk)` or `journal: stopped (code)`.
    /// Never names a key, a path or an entry.
    #[must_use]
    pub fn readiness(&self, policy: &str) -> String {
        if let Some(code) = self.stopped {
            return format!("journal: stopped ({code}), needs a person");
        }
        if self.degraded {
            return format!(
                "journal: degraded (not shipping for {}s, RPO risk)",
                self.degraded_secs
            );
        }
        if !self.writer {
            return format!("journal: {policy} standby (another node ships)");
        }
        format!(
            "journal: {policy} ok (lag {}s, shipped g{}:s{})",
            self.lag_secs, self.durable.generation, self.durable.seq
        )
    }
}

/// A reader's handle on the shipper's progress. Clone it freely.
#[derive(Clone, Debug)]
pub struct ProgressHandle {
    receiver: watch::Receiver<Progress>,
}

impl ProgressHandle {
    /// A handle that never moves: the progress of a journal that is off.
    #[must_use]
    pub fn idle() -> Self {
        let (_sender, receiver) = watch::channel(Progress::initial());
        Self { receiver }
    }

    pub(crate) fn new(receiver: watch::Receiver<Progress>) -> Self {
        Self { receiver }
    }

    /// The progress now.
    #[must_use]
    pub fn now(&self) -> Progress {
        self.receiver.borrow().clone()
    }

    /// The last durable position.
    #[must_use]
    pub fn durable(&self) -> Position {
        self.receiver.borrow().durable
    }

    /// Waits until the durable position reaches `target` (same generation, `seq` at or above,
    /// or a later generation), or `timeout` passes. `true` when it was reached.
    pub async fn wait_durable(&self, target: Position, timeout: Duration) -> bool {
        let mut receiver = self.receiver.clone();
        let reached = |progress: &Progress| {
            (progress.durable.generation, progress.durable.seq) >= (target.generation, target.seq)
        };
        let wait = async {
            loop {
                if reached(&receiver.borrow_and_update()) {
                    return true;
                }
                if receiver.changed().await.is_err() {
                    return false;
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.unwrap_or(false)
    }
}

pub(crate) fn channel() -> (watch::Sender<Progress>, ProgressHandle) {
    let (sender, receiver) = watch::channel(Progress::initial());
    (sender, ProgressHandle::new(receiver))
}
