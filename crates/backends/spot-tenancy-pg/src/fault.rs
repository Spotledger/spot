//! Test seams for the kill-and-resume tests.
//!
//! Production control planes never arm these: a fault is only ever observed once and then
//! disarmed, so an unarmed [`Faults`] is inert. They exist because the acceptance for this task
//! requires aborting a provisioning run at each step *by dropping the connection* and then
//! resuming it; the same seam is what `spot-pg-migrate` offers as `ApplyOptions::stop_after_step`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Armed faults, shared by the control store and the provisioner.
#[derive(Debug, Default)]
pub struct Faults {
    step: Mutex<Option<&'static str>>,
    next_update: AtomicBool,
}

impl Faults {
    /// An unarmed set of faults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops the connection once, after the step called `step` has applied its effect.
    pub fn kill_at(&self, step: Option<&'static str>) {
        if let Ok(mut slot) = self.step.lock() {
            *slot = step;
        }
    }

    /// Drops the control-store connection once, on the next `update`.
    pub fn kill_next_update(&self) {
        self.next_update.store(true, Ordering::SeqCst);
    }

    /// Whether the fault armed for `step` should fire now; it fires once.
    pub(crate) fn take_step(&self, step: &str) -> bool {
        let Ok(mut slot) = self.step.lock() else {
            return false;
        };
        if slot.as_deref() == Some(step) {
            *slot = None;
            true
        } else {
            false
        }
    }

    /// Whether the update fault should fire now; it fires once.
    pub(crate) fn take_update(&self) -> bool {
        self.next_update.swap(false, Ordering::SeqCst)
    }
}
