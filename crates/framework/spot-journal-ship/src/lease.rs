//! The one-writer rule over the site's coordinator: a named lease with a fencing token.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use spot_coordinator::{Coordinator, Lease};

use crate::ports::{PortFuture, WriterLease};

/// Holds the lease `name` for `holder` in `coordinator`, renewing it on every [`hold`] call.
///
/// A node that loses the lease answers `false` from the next call, so it stops shipping within
/// one lease interval. Every call also checks the fencing token, so a deposed writer is noticed
/// even before the lease times out.
///
/// [`hold`]: WriterLease::hold
pub struct CoordinatorLease {
    coordinator: Arc<dyn Coordinator>,
    name: String,
    holder: String,
    ttl: Duration,
    held: Mutex<Option<Lease>>,
}

impl CoordinatorLease {
    /// A lease named `name` held for `ttl` by `holder`.
    #[must_use]
    pub fn new(
        coordinator: Arc<dyn Coordinator>,
        name: String,
        holder: String,
        ttl: Duration,
    ) -> Self {
        Self {
            coordinator,
            name,
            holder,
            ttl,
            held: Mutex::new(None),
        }
    }

    fn handle(&self) -> Option<Lease> {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn put(&self, lease: Option<Lease>) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = lease;
    }
}

impl WriterLease for CoordinatorLease {
    fn hold(&self) -> PortFuture<'_, bool> {
        Box::pin(async move {
            if let Some(lease) = self.handle() {
                // Still ours? The fence is the check; the renewal keeps it alive.
                if self
                    .coordinator
                    .check_fence(&self.name, lease.token)
                    .await
                    .is_ok()
                {
                    return match self.coordinator.renew(&lease, self.ttl).await {
                        Ok(renewed) => {
                            self.put(Some(renewed));
                            true
                        }
                        Err(_) => {
                            self.put(None);
                            false
                        }
                    };
                }
                self.put(None);
            }
            match self
                .coordinator
                .acquire(&self.name, &self.holder, self.ttl)
                .await
            {
                Ok(lease) => {
                    self.put(Some(lease));
                    true
                }
                Err(_) => false,
            }
        })
    }

    fn release(&self) -> PortFuture<'_, ()> {
        Box::pin(async move {
            let lease = self
                .held
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(lease) = lease {
                let _ = self.coordinator.release(&lease).await;
            }
        })
    }
}
