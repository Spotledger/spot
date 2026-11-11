//! Quiescing the source: the maintenance gate and the side-effect hold.
//!
//! While a move is between `Quiesced` and `Cleaned`, no write may land on the source and no
//! held-class external call may leave it. The engine drives a [`QuiescePort`]; the production
//! adapter ([`LifecycleQuiesce`]) closes the same `AdmissionGate` the migration engine uses
//! (WP-MIG-07) and begins the same `spot-effect-hold` `HoldService` the migration lifecycle
//! uses (WP-MIG-08), with the move id as the hold's migration id.

use std::sync::Arc;

use spot_effect_hold::{Error as HoldError, HoldService, ReleasePlan};
use spot_migration_apply::AdmissionGate;

use crate::BoxFuture;
use crate::error::{Error, Result};
use crate::plan::{MoveId, MovePlan};

/// How a move quiesces and resumes its source.
///
/// [`quiesce`](QuiescePort::quiesce) must not return until new writes are refused and every
/// external effect that is about to be sent is durably held. [`resume`](QuiescePort::resume)
/// must be idempotent: it is called after cut-over and on abort, and may be called again
/// after a crash.
pub trait QuiescePort: Send + Sync {
    /// Closes the gate and begins the hold for `move_id`.
    ///
    /// # Errors
    /// [`Error::QuiesceFailed`] with a fixed reason.
    fn quiesce<'a>(
        &'a self,
        plan: &'a MovePlan,
        move_id: &'a MoveId,
        now: u64,
    ) -> BoxFuture<'a, Result<()>>;

    /// Releases the hold and opens the gate. Idempotent.
    ///
    /// # Errors
    /// [`Error::QuiesceFailed`] with a fixed reason.
    fn resume<'a>(
        &'a self,
        plan: &'a MovePlan,
        move_id: &'a MoveId,
        now: u64,
    ) -> BoxFuture<'a, Result<()>>;
}

/// The production quiesce adapter over the migration lifecycle's gate and hold.
pub struct LifecycleQuiesce {
    gate: Arc<dyn AdmissionGate>,
    hold: Arc<HoldService>,
}

impl LifecycleQuiesce {
    /// An adapter over an admission gate and a hold service for the source site.
    #[must_use]
    pub fn new(gate: Arc<dyn AdmissionGate>, hold: Arc<HoldService>) -> Self {
        Self { gate, hold }
    }
}

/// Our records use Unix milliseconds; the migration gate and hold use microseconds.
fn micros(now: u64) -> i64 {
    i64::try_from(now.saturating_mul(1_000)).unwrap_or(i64::MAX)
}

impl QuiescePort for LifecycleQuiesce {
    fn quiesce<'a>(
        &'a self,
        plan: &'a MovePlan,
        move_id: &'a MoveId,
        now: u64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let at = micros(now);
            self.gate
                .close("tenant move", at)
                .await
                .map_err(|_| Error::QuiesceFailed {
                    tenant: plan.tenant.to_string(),
                    reason: "the admission gate could not be closed",
                })?;
            if let Err(error) = self
                .hold
                .begin(move_id.as_str(), "tenant move", at, None)
                .await
            {
                // A retried step after a crash may find the same hold already live.
                if !matches!(
                    &error,
                    HoldError::AlreadyHeld { migration_id, .. } if migration_id == move_id.as_str()
                ) {
                    // Fail closed: leaving the gate closed is the safe side of this error.
                    let _ = self.gate.open(at).await;
                    return Err(Error::QuiesceFailed {
                        tenant: plan.tenant.to_string(),
                        reason: "the side-effect hold could not begin",
                    });
                }
            }
            Ok(())
        })
    }

    fn resume<'a>(
        &'a self,
        plan: &'a MovePlan,
        move_id: &'a MoveId,
        now: u64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let at = micros(now);
            match self
                .hold
                .release(move_id.as_str(), ReleasePlan::new(), at)
                .await
            {
                Ok(report) => {
                    if !report.completed
                        || !report.failed.is_empty()
                        || !report.needs_confirmation.is_empty()
                    {
                        return Err(Error::QuiesceFailed {
                            tenant: plan.tenant.to_string(),
                            reason: "the side-effect release needs operator confirmation",
                        });
                    }
                }
                // Already released (a retried cleanup or an abort before the hold began).
                Err(HoldError::NotHeld { .. }) => {}
                Err(_) => {
                    return Err(Error::QuiesceFailed {
                        tenant: plan.tenant.to_string(),
                        reason: "the side-effect hold could not be released",
                    });
                }
            }
            self.gate.open(at).await.map_err(|_| Error::QuiesceFailed {
                tenant: plan.tenant.to_string(),
                reason: "the admission gate could not be opened",
            })
        })
    }
}
