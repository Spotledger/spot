//! The commit request, its ticket and the answers a ticket resolves with.

use spot_journal_durability::{Refusal, Seq};
use tokio::sync::oneshot;

/// One commit to make durable and acknowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRequest {
    /// The caller's idempotency key (ADR-056). Only compared, never logged.
    pub command_id: String,
    /// The commit's sequence number; strictly consecutive across requests.
    pub seq: Seq,
    /// The envelope as one line of UTF-8 (opaque to the driver).
    pub entry: Vec<u8>,
}

/// What a ticket resolves with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitAnswer {
    /// The policy condition holds: the commit may be acknowledged to the caller.
    Acked(Seq),
    /// The commit was not started (`halt` with the store unreachable): nothing was committed.
    Refused(Refusal),
    /// The commit is durable locally but its acknowledgement did not arrive in time. Retry with
    /// the same command id: the retry queries the outcome and never commits twice.
    OutcomeUnknown {
        /// The commit's sequence number.
        seq: Seq,
    },
}

/// One future per commit.
#[derive(Debug)]
pub struct CommitTicket {
    seq: Seq,
    rx: oneshot::Receiver<CommitAnswer>,
}

impl CommitTicket {
    pub(crate) fn new(seq: Seq, rx: oneshot::Receiver<CommitAnswer>) -> Self {
        Self { seq, rx }
    }

    /// The sequence number this ticket is for.
    #[must_use]
    pub fn seq(&self) -> Seq {
        self.seq
    }

    /// Waits for the answer. If the driver is dropped first the outcome is reported unknown.
    pub async fn wait(self) -> CommitAnswer {
        let seq = self.seq;
        self.rx
            .await
            .unwrap_or(CommitAnswer::OutcomeUnknown { seq })
    }

    /// The answer if it is already there.
    pub fn try_answer(&mut self) -> Option<CommitAnswer> {
        self.rx.try_recv().ok()
    }
}

/// Sees every resolution in the order the driver makes it (metrics, tests).
pub trait Observer: Send + Sync {
    /// A commit was resolved with `answer`.
    fn resolved(&self, seq: Seq, answer: &CommitAnswer);
}
