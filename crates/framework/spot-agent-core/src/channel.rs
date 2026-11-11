//! The local channel between the unprivileged broker and the privileged executor.
//!
//! The broker and the executor run as separate privileges but on the same host; the only
//! thing that crosses between them is a [`PlanEnvelope`]. [`LocalChannel`] is the trait a
//! host implements over whatever local transport it uses (a Unix domain socket, a pipe);
//! [`InProcessChannel`] is the in-process pair used by tests and by a single-process
//! deployment.
//!
//! Nothing here grants authority: the executor independently re-derives the plan from the
//! command the envelope carries, so a message arriving on this channel is a *request* to
//! act, not permission to act.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::error::Refusal;
use crate::executor::PlanEnvelope;

/// The local channel a broker sends plans over and an executor receives them from.
pub trait LocalChannel: Send + Sync {
    /// Sends one plan.
    ///
    /// # Errors
    /// [`Refusal::LogUnavailable`] when the transport fails (the channel is a local
    /// transport, so this is the generic "the local side failed" refusal).
    fn send(&self, plan: PlanEnvelope) -> Result<(), Refusal>;

    /// Receives the next plan, or `None` when the channel is empty.
    ///
    /// # Errors
    /// [`Refusal::LogUnavailable`] when the transport fails.
    fn recv(&self) -> Result<Option<PlanEnvelope>, Refusal>;
}

/// An in-process channel: a mutex-guarded queue shared by both ends.
#[derive(Default)]
pub struct InProcessChannel {
    queue: Mutex<VecDeque<PlanEnvelope>>,
}

impl InProcessChannel {
    /// A new empty channel, as the shared handle both ends hold.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// How many plans are queued.
    ///
    /// # Errors
    /// [`Refusal::LogUnavailable`] when the lock is poisoned.
    pub fn len(&self) -> Result<usize, Refusal> {
        Ok(self
            .queue
            .lock()
            .map_err(|_| Refusal::LogUnavailable)?
            .len())
    }

    /// Whether the channel is empty.
    ///
    /// # Errors
    /// [`Refusal::LogUnavailable`] when the lock is poisoned.
    pub fn is_empty(&self) -> Result<bool, Refusal> {
        Ok(self
            .queue
            .lock()
            .map_err(|_| Refusal::LogUnavailable)?
            .is_empty())
    }
}

impl LocalChannel for InProcessChannel {
    fn send(&self, plan: PlanEnvelope) -> Result<(), Refusal> {
        self.queue
            .lock()
            .map_err(|_| Refusal::LogUnavailable)?
            .push_back(plan);
        Ok(())
    }

    fn recv(&self) -> Result<Option<PlanEnvelope>, Refusal> {
        Ok(self
            .queue
            .lock()
            .map_err(|_| Refusal::LogUnavailable)?
            .pop_front())
    }
}

impl std::fmt::Debug for InProcessChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcessChannel")
            .field("queued", &self.queue.lock().map(|q| q.len()).unwrap_or(0))
            .finish()
    }
}
