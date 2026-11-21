//! The timer adapter: registering, clearing and firing a Case's waits under the scheduler's
//! lease and fencing model.
//!
//! A [`TimerPort`] is the engine's route to `spot-scheduler` (WP-PLT-07). [`FencedTimerPort`]
//! wraps another port and lets it act **only while the caller still holds the schedule-leader
//! lease**: every method runs [`FencedWrite::check`] first, so a node that was paused past its
//! TTL and lost the lease can neither register a wait nor fire one — the new leader's port is the
//! only one that does. The durable wait itself lives in the Case store
//! ([`TimerRecord`](spot_bpm_engine::TimerRecord)); this port is the scheduler-facing route, so a
//! stale node firing is harmless and a missed fire is caught by the next `step`.

use std::sync::Arc;

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, TimerPort, TimerRegistration};
use spot_dag::NodeId;
use spot_scheduler::{FencedWrite, Lease, LeaseStore};

/// A [`TimerPort`] guarded by a scheduler lease.
pub struct FencedTimerPort {
    leases: Arc<dyn LeaseStore>,
    lease: Lease,
    inner: Arc<dyn TimerPort>,
}

impl std::fmt::Debug for FencedTimerPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FencedTimerPort")
            .field("lease", &self.lease.name)
            .finish_non_exhaustive()
    }
}

impl FencedTimerPort {
    /// A port that acts on `inner` only while `lease` is still the store's current record.
    #[must_use]
    pub fn new(leases: Arc<dyn LeaseStore>, lease: Lease, inner: Arc<dyn TimerPort>) -> Self {
        Self {
            leases,
            lease,
            inner,
        }
    }

    /// Whether this holder still holds the lease, so a write may proceed.
    #[must_use]
    pub fn holds(&self) -> bool {
        FencedWrite::check(&*self.leases, &self.lease).is_ok()
    }
}

impl TimerPort for FencedTimerPort {
    fn register(&self, registration: TimerRegistration) {
        if self.holds() {
            self.inner.register(registration);
        }
    }

    fn clear(&self, case: &CaseId, node: &NodeId) {
        if self.holds() {
            self.inner.clear(case, node);
        }
    }

    fn due(&self, now: OffsetDateTime) -> Vec<TimerRegistration> {
        if self.holds() {
            self.inner.due(now)
        } else {
            Vec::new()
        }
    }

    fn cancel_case(&self, case: &CaseId) {
        if self.holds() {
            self.inner.cancel_case(case);
        }
    }

    fn fired_count(&self, case: &CaseId, node: &NodeId) -> u32 {
        self.inner.fired_count(case, node)
    }
}
