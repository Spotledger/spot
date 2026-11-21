//! The one shared egress handle (WP-SEC-09.T10).
//!
//! `spot_egress::Guard` holds its policy by value, so a reload cannot change a guard somebody
//! already cloned. [`SharedGuard`] is the handle services keep instead: its identity never
//! changes, and the guard behind it is swapped atomically on a reload. Every decision takes one
//! snapshot of the guard (a single `Arc` clone under a read lock) and decides on that snapshot
//! alone, so a decision made during a swap sees the old policy or the new one, never a mix.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};

use spot_egress::{Authorized, EgressError, Guard, Purpose, Request};

/// How many swap records are kept in memory (the audit line is written for every one).
pub const SWAP_LOG_KEEP: usize = 32;

/// The audit record of one applied policy swap: names and counts only. No allow-list entry,
/// host name or address is in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapAudit {
    /// The generation now in force (1 is the policy at start).
    pub generation: u64,
    /// The first 12 hex digits of the new policy's SHA-256.
    pub digest: String,
    /// How many tenants the new policy lists.
    pub tenants: usize,
    /// How many tenants the replaced policy listed.
    pub previous_tenants: usize,
}

/// The audit target the swap line is written to.
pub const AUDIT_TARGET: &str = "spot::audit::egress_policy";

/// The handle to the guard in force.
pub struct SharedGuard {
    current: RwLock<Arc<Guard>>,
    swaps: Mutex<VecDeque<SwapAudit>>,
}

impl SharedGuard {
    /// A handle whose first guard is `guard`.
    pub fn new(guard: Arc<Guard>) -> Self {
        Self {
            current: RwLock::new(guard),
            swaps: Mutex::new(VecDeque::new()),
        }
    }

    /// The guard in force now. Hold it only for one decision.
    pub fn current(&self) -> Arc<Guard> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// [`Guard::authorize`] on the guard in force.
    ///
    /// # Errors
    ///
    /// The guard's refusal.
    pub async fn authorize(&self, req: &Request) -> Result<Authorized, EgressError> {
        let guard = self.current();
        guard.authorize(req).await
    }

    /// [`Guard::authorize_addr`] on the guard in force.
    ///
    /// # Errors
    ///
    /// The guard's refusal.
    pub async fn authorize_addr(
        &self,
        host: &str,
        port: u16,
        purpose: Purpose,
        tenant: &str,
        actor: Option<String>,
    ) -> Result<Authorized, EgressError> {
        let guard = self.current();
        guard
            .authorize_addr(host, port, purpose, tenant, actor)
            .await
    }

    /// [`Guard::authorize_uri`] on the guard in force.
    ///
    /// # Errors
    ///
    /// The guard's refusal.
    pub async fn authorize_uri(
        &self,
        uri: &http::Uri,
        purpose: Purpose,
        tenant: &str,
        actor: Option<String>,
    ) -> Result<Authorized, EgressError> {
        let guard = self.current();
        guard.authorize_uri(uri, purpose, tenant, actor).await
    }

    /// Puts `guard` in force and records the swap (one audit line, one in-memory record).
    pub(super) fn swap(&self, guard: Arc<Guard>, audit: SwapAudit) {
        {
            let mut current = self.current.write().unwrap_or_else(|e| e.into_inner());
            *current = guard;
        }
        tracing::info!(
            target: "spot::audit::egress_policy",
            generation = audit.generation,
            digest = %audit.digest,
            tenants = audit.tenants,
            previous_tenants = audit.previous_tenants,
            "egress policy swapped"
        );
        let mut swaps = self.swaps.lock().unwrap_or_else(|e| e.into_inner());
        if swaps.len() >= SWAP_LOG_KEEP {
            swaps.pop_front();
        }
        swaps.push_back(audit);
    }

    /// The most recent swap records, oldest first (at most [`SWAP_LOG_KEEP`]).
    pub fn swaps(&self) -> Vec<SwapAudit> {
        self.swaps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}
