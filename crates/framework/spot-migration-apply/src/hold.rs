//! The side-effect hold the engine drives.
//!
//! Before the first step the engine begins the hold (`spot-effect-hold`'s
//! [`HoldService::begin`](spot_effect_hold::HoldService::begin)); after verification it releases
//! the held effects, and after a rollback it discards them. The hold service marks its shared
//! `Gate` held, which is what [`QueueGate`](spot_effect_hold::QueueGate) and
//! [`EdgeSyncGate`](spot_effect_hold::EdgeSyncGate) consult, so jobs and edge sync pause for the
//! whole window.

use std::sync::Mutex;

use spot_effect_hold::{EdgeSyncGate, HoldService, QueueGate, ReleasePlan as EffectReleasePlan};

use crate::BoxFuture;
use crate::error::{Error, Result};

/// Begins, releases and discards the side-effect hold for one migration.
pub trait SideEffectHold: Send + Sync {
    /// Begins holding the site while `migration_id` is unverified.
    ///
    /// Beginning a hold that is already live for the same migration is a no-op, so a restart can
    /// call it again.
    ///
    /// # Errors
    /// A backend error when the hold cannot be begun.
    fn begin<'a>(
        &'a self,
        migration_id: &'a str,
        reason: &'a str,
        now: i64,
    ) -> BoxFuture<'a, Result<()>>;

    /// Releases the held effects to `migration_id`.
    ///
    /// # Errors
    /// A backend error when the release fails; the hold is unchanged and the site stays closed.
    fn release<'a>(&'a self, migration_id: &'a str, now: i64) -> BoxFuture<'a, Result<()>>;

    /// Discards the held effects for `migration_id`.
    ///
    /// # Errors
    /// A backend error when the discard fails.
    fn discard<'a>(
        &'a self,
        migration_id: &'a str,
        reason: &'a str,
        now: i64,
    ) -> BoxFuture<'a, Result<()>>;
}

/// The real hold: a thin adapter over [`HoldService`].
pub struct HoldServiceBridge {
    service: std::sync::Arc<HoldService>,
    expected_by: Option<i64>,
}

impl HoldServiceBridge {
    /// A bridge over `service`.
    #[must_use]
    pub fn new(service: std::sync::Arc<HoldService>) -> Self {
        Self {
            service,
            expected_by: None,
        }
    }

    /// The same bridge recording an expected verification time.
    #[must_use]
    pub fn with_expected_by(mut self, expected_by: i64) -> Self {
        self.expected_by = Some(expected_by);
        self
    }

    /// The job worker's gate: `is_claimable` is `false` while the site is held.
    #[must_use]
    pub fn queue_gate(&self) -> QueueGate {
        QueueGate::new(self.service.gate())
    }

    /// The edge-sync gate: `may_send` and `may_apply` are `false` while the site is held.
    #[must_use]
    pub fn edge_sync_gate(&self) -> EdgeSyncGate {
        EdgeSyncGate::new(self.service.gate())
    }
}

impl SideEffectHold for HoldServiceBridge {
    fn begin<'a>(
        &'a self,
        migration_id: &'a str,
        reason: &'a str,
        now: i64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match self
                .service
                .begin(migration_id, reason, now, self.expected_by)
                .await
            {
                Ok(_) => Ok(()),
                // A restart after a crash finds the durable hold already live for this migration:
                // beginning it again is a no-op, not a failure.
                Err(spot_effect_hold::Error::AlreadyHeld {
                    migration_id: held, ..
                }) if held == migration_id => Ok(()),
                Err(error) => Err(Error::HoldFailed {
                    message: error.to_string(),
                }),
            }
        })
    }

    fn release<'a>(&'a self, migration_id: &'a str, now: i64) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match self
                .service
                .release(migration_id, EffectReleasePlan::new(), now)
                .await
            {
                Ok(_) => Ok(()),
                // A restart may find the release already done; releasing an open site is a no-op.
                Err(spot_effect_hold::Error::NotHeld { .. }) => Ok(()),
                Err(error) => Err(Error::ReleaseFailed {
                    message: error.to_string(),
                }),
            }
        })
    }

    fn discard<'a>(
        &'a self,
        migration_id: &'a str,
        reason: &'a str,
        now: i64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            match self.service.discard(migration_id, reason, now).await {
                Ok(_) => Ok(()),
                // A restart may find the discard already done; discarding an open site is a no-op.
                Err(spot_effect_hold::Error::NotHeld { .. }) => Ok(()),
                Err(error) => Err(Error::HoldFailed {
                    message: error.to_string(),
                }),
            }
        })
    }
}

/// An in-memory hold that records the calls, for tests and for a host before the real service is
/// wired.
#[derive(Debug, Default)]
pub struct RecordingHold {
    held: Mutex<Option<String>>,
    log: Mutex<Vec<String>>,
}

impl RecordingHold {
    /// An open site with no hold.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The migration currently held, if any.
    #[must_use]
    pub fn held(&self) -> Option<String> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The calls made, in order (`begin:<id>`, `release:<id>`, `discard:<id>`).
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn record(&self, line: String) {
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(line);
    }
}

impl SideEffectHold for RecordingHold {
    fn begin<'a>(
        &'a self,
        migration_id: &'a str,
        _reason: &'a str,
        _now: i64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut held = self
                .held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match held.as_deref() {
                Some(live) if live == migration_id => Ok(()),
                Some(live) => Err(Error::HoldFailed {
                    message: format!("the site is already held for migration {live}"),
                }),
                None => {
                    *held = Some(migration_id.to_owned());
                    drop(held);
                    self.record(format!("begin:{migration_id}"));
                    Ok(())
                }
            }
        })
    }

    fn release<'a>(&'a self, migration_id: &'a str, _now: i64) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut held = self
                .held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match held.as_deref() {
                Some(live) if live == migration_id => {
                    *held = None;
                    drop(held);
                    self.record(format!("release:{migration_id}"));
                    Ok(())
                }
                // Already released (a restart): releasing an open site is a no-op.
                None => Ok(()),
                Some(live) => Err(Error::ReleaseFailed {
                    message: format!("the site is held for migration {live}"),
                }),
            }
        })
    }

    fn discard<'a>(
        &'a self,
        migration_id: &'a str,
        _reason: &'a str,
        _now: i64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut held = self
                .held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match held.as_deref() {
                Some(live) if live == migration_id => {
                    *held = None;
                    drop(held);
                    self.record(format!("discard:{migration_id}"));
                    Ok(())
                }
                // Already discarded (a restart): discarding an open site is a no-op.
                None => Ok(()),
                Some(live) => Err(Error::HoldFailed {
                    message: format!("the site is held for migration {live}"),
                }),
            }
        })
    }
}
