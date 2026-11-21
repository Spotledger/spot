//! Providers for ports of the link services that need no new dependency (WP-CTL-10.T10): the
//! effect-hold probe adapter, the resilience factory and (in `link_ports/nonce_pg.rs`) the
//! PostgreSQL nonce store of the external link.
//!
//! What is **not** here, and why, is listed in `docs/running-spot/link_ports.md`: the
//! mutual-TLS transport, the peer directory, the replica-apply adapter and the usage counters
//! need crates or engine seams outside this task's paths.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_link_transport::Resilience;
use spot_sync_http::resilience::{SeededJitter, TokioSleeper};
use tokio::sync::Semaphore;

use super::link_exchange::{ResilienceFactory, SendHold};

#[path = "link_ports/nonce_pg.rs"]
pub mod nonce_pg;

pub use nonce_pg::PgNonceStore;

/// A [`SendHold`] over a probe of the effect hold's state.
///
/// The probe answers `Some(true)` when external calls are held, `Some(false)` when the site is
/// open and `None` when the state cannot be read; **unknown is held** (fail closed), exactly like
/// the gate's own `Unknown` snapshot. The host passes
/// `|| Some(!matches!(gate.snapshot(), HoldSnapshot::Open))` over its `spot_effect_hold::Gate`.
pub struct ProbeHold<F> {
    probe: F,
}

impl<F: Fn() -> Option<bool> + Send + Sync> ProbeHold<F> {
    /// A hold over `probe`.
    pub fn new(probe: F) -> Self {
        Self { probe }
    }
}

impl<F: Fn() -> Option<bool> + Send + Sync> SendHold for ProbeHold<F> {
    fn held(&self) -> bool {
        (self.probe)().unwrap_or(true)
    }
}

/// Builds the resilience kit of each link's client: a seeded jitter (the seed is injected by the
/// host; each kit gets its own derived seed so links do not back off in step) and the Tokio timer.
pub struct SeededResilience {
    seed: u64,
    made: AtomicU64,
}

impl SeededResilience {
    /// A factory from the host's seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            made: AtomicU64::new(0),
        }
    }
}

impl ResilienceFactory for SeededResilience {
    fn make(&self, bulkhead: Arc<Semaphore>) -> Resilience {
        let n = self.made.fetch_add(1, Ordering::Relaxed);
        Resilience {
            jitter: Box::new(SeededJitter::new(
                self.seed
                    .wrapping_add(n.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
            )),
            sleeper: Arc::new(TokioSleeper),
            bulkhead,
        }
    }
}
