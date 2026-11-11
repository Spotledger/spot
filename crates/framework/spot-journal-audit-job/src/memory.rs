//! In-memory providers for the tests and for a caller that needs a reference.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, PoisonError};

use spot_journal_audit_run::Clock;
use spot_journal_store::TenantId;

use crate::cadence::{Cadence, TenantAuditPlan};
use crate::error::AuditJobError;
use crate::providers::{AuditInputs, ResultSink, TenantDirectory, TenantInputs};
use crate::run::TenantResult;

/// A directory and a result sink in one: it keeps the schedule and, when a result is recorded,
/// moves the tenant's `last_run_millis` to that run's start, as the real pair must.
#[derive(Debug, Default)]
pub struct MemoryBook {
    plans: Mutex<Vec<TenantAuditPlan>>,
    results: Mutex<Vec<TenantResult>>,
    fail_directory: Mutex<bool>,
}

impl MemoryBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tenant with its cadence and last run (an invalid tenant id is ignored).
    pub fn add(&self, tenant: &str, cadence: Cadence, last_run_millis: Option<i64>) {
        if let Ok(tenant) = TenantId::new(tenant) {
            self.plans
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(TenantAuditPlan {
                    tenant,
                    cadence,
                    last_run_millis,
                });
        }
    }

    /// Makes the directory fail (or answer again).
    pub fn set_directory_failing(&self, failing: bool) {
        *self
            .fail_directory
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = failing;
    }

    /// Every result recorded so far, oldest first.
    #[must_use]
    pub fn results(&self) -> Vec<TenantResult> {
        self.results
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl TenantDirectory for MemoryBook {
    fn tenants(&self) -> Result<Vec<TenantAuditPlan>, AuditJobError> {
        if *self
            .fail_directory
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return Err(AuditJobError::ProviderFailed {
                provider: "directory",
            });
        }
        Ok(self
            .plans
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }
}

impl ResultSink for MemoryBook {
    fn record(&self, tenant: &TenantId, result: &TenantResult) -> Result<(), AuditJobError> {
        for plan in self
            .plans
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter_mut()
            .filter(|plan| &plan.tenant == tenant)
        {
            plan.last_run_millis = Some(result.started_millis);
        }
        self.results
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(result.clone());
        Ok(())
    }
}

/// Inputs built by a closure, one call per tenant.
pub struct FnInputs<F>(F);

impl<F> FnInputs<F>
where
    F: Fn(&TenantId) -> Result<TenantInputs, AuditJobError> + Send + Sync,
{
    /// Inputs that `open` calls `f` for.
    pub fn new(f: F) -> Self {
        Self(f)
    }
}

impl<F> AuditInputs for FnInputs<F>
where
    F: Fn(&TenantId) -> Result<TenantInputs, AuditJobError> + Send + Sync,
{
    fn open(&self, tenant: &TenantId) -> Result<TenantInputs, AuditJobError> {
        (self.0)(tenant)
    }
}

/// A clock that advances by a fixed step on every read and can be shared across threads.
#[derive(Debug)]
pub struct SharedClock {
    now: AtomicI64,
    step: i64,
}

impl SharedClock {
    /// A clock that starts at `start` and moves `step` milliseconds forward on each read.
    #[must_use]
    pub fn new(start: i64, step: i64) -> Self {
        Self {
            now: AtomicI64::new(start),
            step,
        }
    }
}

impl Clock for SharedClock {
    fn now_millis(&self) -> i64 {
        self.now.fetch_add(self.step, Ordering::Relaxed)
    }
}
