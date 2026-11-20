//! The cluster rate gate of the function service (WP-PLT-17.T8).
//!
//! A function's declared `limit = { permits = 10, per = "1m" }` is enforced by the
//! [`RateGate`] its registry holds. The default gate is the node's own token bucket, which on
//! three nodes admits thirty calls a minute. In the `cluster` profile this module installs a
//! gate over the coordinator's `take` instead, so the limit holds for the site.
//!
//! - [`CoordinatorGate`]: one coordinator rate bucket per `(site, function, caller)`. The key is
//!   `function:<site>:<function>:<caller hash>`, where the caller part is the first 16 bytes of
//!   the SHA-256 of the caller's bucket label in hex, so a key in a `spot_coord_rate` row or a
//!   Redis `MONITOR` names no user. Capacity is `permits`, the window `per`, the cost 1.
//! - **Fallback, stricter and never open.** A coordinator that cannot answer (unavailable, lease
//!   lost, anything) makes that call fall back to the node's own [`LocalGate`]: the function is
//!   then limited to at most `permits` per node, which is looser than the cluster limit only in
//!   the sense that three nodes may admit three times as much, and never unlimited. The fallback
//!   is counted (`spot_function_gate_fallback_total`) and logged at `warn` once a minute.
//! - [`MeteredGate`]: wraps whichever gate is installed and records the gate's own latency
//!   (`spot_function_gate_seconds`) and every refusal (`spot_function_rate_limited_total`). No
//!   function name and no caller is ever a label: a function name is app text and a caller is a
//!   user.
//! - [`GateInstaller`]: what the function service holds. [`GateInstaller::apply`] runs on every
//!   registry it builds, at assembly and on every reload, so a reload never drops the gate. The
//!   single-node profile gets the local gate (a database round trip per call buys nothing there),
//!   still metered.
//!
//! This module is a helper: it declares no `ORDER` and registers nothing. The function service
//! calls [`coordinator_for`] once when the site is assembled and [`GateInstaller::apply`] at the
//! end of every registry build.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use spot_coordinator::types::MAX_NAME_BYTES;
use spot_coordinator::{Admit, Coordinator, RateLimit};
use spot_core::clock::Clock;
use spot_functions::{FunctionRegistry, GateFuture, GateKey, LimitError, LocalGate, RateGate};

use crate::services::SiteCtx;
use crate::{Error, Result};

/// Counter: calls refused by a function's rate limit. No labels.
pub const RATE_LIMITED_TOTAL: &str = "spot_function_rate_limited_total";
/// Counter: calls where the coordinator could not answer and the node's own bucket decided.
pub const GATE_FALLBACK_TOTAL: &str = "spot_function_gate_fallback_total";
/// Histogram: how long the gate took to answer, in seconds.
pub const GATE_SECONDS: &str = "spot_function_gate_seconds";

/// How often the fallback is logged at `warn`.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// The profile name that shares limits across nodes.
const CLUSTER_PROFILE: &str = "cluster";

/// Lower-case hex of the first 16 bytes of the SHA-256 of `text`: fixed length, no user id.
fn short_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The coordinator key of one `(site, function, caller)` bucket:
/// `function:<site>:<function>:<caller hash>`. When the key would be longer than a coordinator
/// name may be, the function part is hashed the same way too.
pub fn coordinator_key(site: &str, key: &GateKey) -> String {
    let caller = short_hash(&key.caller);
    let plain = format!("function:{site}:{}:{caller}", key.function);
    if plain.len() <= MAX_NAME_BYTES {
        plain
    } else {
        format!("function:{site}:{}:{caller}", short_hash(&key.function))
    }
}

/// The gate over the coordinator's `take`, with the node's own bucket as the fallback.
pub struct CoordinatorGate {
    coordinator: Arc<dyn Coordinator>,
    site: String,
    fallback: Arc<LocalGate>,
    clock: Arc<dyn Clock>,
    fallbacks: AtomicU64,
    last_warn: Mutex<Option<Duration>>,
}

impl CoordinatorGate {
    /// A gate for `site` over `coordinator`. `fallback` answers when the coordinator cannot
    /// (the registry's [`FunctionRegistry::local_gate`]); `clock` times the fallback warning.
    pub fn new(
        coordinator: Arc<dyn Coordinator>,
        site: &str,
        fallback: Arc<LocalGate>,
        clock: Arc<dyn Clock>,
    ) -> CoordinatorGate {
        CoordinatorGate {
            coordinator,
            site: site.to_owned(),
            fallback,
            clock,
            fallbacks: AtomicU64::new(0),
            last_warn: Mutex::new(None),
        }
    }

    /// How many calls have fallen back to the node's own bucket since this gate was built.
    pub fn fallbacks(&self) -> u64 {
        self.fallbacks.load(Ordering::Relaxed)
    }

    /// Counts one fallback and logs it at `warn` at most once a minute.
    fn note_fallback(&self, code: &str) {
        self.fallbacks.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(GATE_FALLBACK_TOTAL).increment(1);
        let now = self.clock.monotonic();
        let mut last = self
            .last_warn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let due = match *last {
            None => true,
            Some(at) => now.saturating_sub(at) >= WARN_EVERY,
        };
        if due {
            *last = Some(now);
            tracing::warn!(
                site = %self.site,
                code,
                "the function rate gate could not reach the coordinator; this node's own buckets decide"
            );
        }
    }
}

impl RateGate for CoordinatorGate {
    fn admit<'a>(&'a self, key: &'a GateKey, permits: u32, per: Duration) -> GateFuture<'a> {
        Box::pin(async move {
            let name = coordinator_key(&self.site, key);
            let limit = RateLimit {
                capacity: u64::from(permits),
                per,
            };
            match self.coordinator.take(&name, limit, 1).await {
                Ok(Admit::Allowed { .. }) => Ok(()),
                Ok(Admit::Denied { retry_after }) => Err(LimitError { retry_after }),
                Err(error) => {
                    self.note_fallback(error.code);
                    self.fallback.check(key, permits, per)
                }
            }
        })
    }

    fn evict_idle(&self, older_than: Duration) -> usize {
        self.fallback.evict_idle(older_than)
    }

    fn bucket_count(&self) -> usize {
        self.fallback.bucket_count()
    }
}

/// Wraps a gate: records the gate's latency and every refusal, answers exactly as the inner
/// gate does.
pub struct MeteredGate {
    inner: Arc<dyn RateGate>,
    clock: Arc<dyn Clock>,
}

impl MeteredGate {
    /// Meters `inner`, timing with `clock`.
    pub fn new(inner: Arc<dyn RateGate>, clock: Arc<dyn Clock>) -> MeteredGate {
        MeteredGate { inner, clock }
    }
}

impl RateGate for MeteredGate {
    fn admit<'a>(&'a self, key: &'a GateKey, permits: u32, per: Duration) -> GateFuture<'a> {
        Box::pin(async move {
            let started = self.clock.monotonic();
            let verdict = self.inner.admit(key, permits, per).await;
            let took = self.clock.monotonic().saturating_sub(started);
            metrics::histogram!(GATE_SECONDS).record(took.as_secs_f64());
            if verdict.is_err() {
                metrics::counter!(RATE_LIMITED_TOTAL).increment(1);
            }
            verdict
        })
    }

    fn evict_idle(&self, older_than: Duration) -> usize {
        self.inner.evict_idle(older_than)
    }

    fn bucket_count(&self) -> usize {
        self.inner.bucket_count()
    }
}

/// What the function service holds to put the right gate on every registry it builds.
pub struct GateInstaller {
    site: String,
    clock: Arc<dyn Clock>,
    /// Present only in the cluster profile.
    coordinator: Option<Arc<dyn Coordinator>>,
    /// The cluster gates installed so far, for tests and for a status line.
    installed: Mutex<Vec<Arc<CoordinatorGate>>>,
}

impl GateInstaller {
    /// An installer for `site` under `profile`. The `coordinator` is used only when the profile
    /// is `cluster`; under any other profile it is dropped and no coordinator gate is ever built.
    pub fn new(
        profile: &str,
        site: &str,
        clock: Arc<dyn Clock>,
        coordinator: Option<Arc<dyn Coordinator>>,
    ) -> GateInstaller {
        let coordinator = if profile == CLUSTER_PROFILE {
            coordinator
        } else {
            None
        };
        GateInstaller {
            site: site.to_owned(),
            clock,
            coordinator,
            installed: Mutex::new(Vec::new()),
        }
    }

    /// Whether registries get the coordinator gate (the cluster profile with a coordinator).
    pub fn is_shared(&self) -> bool {
        self.coordinator.is_some()
    }

    /// Puts the gate on `registry`: the coordinator gate in the cluster profile, the node's own
    /// bucket otherwise, both metered. Call it on every registry the service builds.
    pub fn apply(&self, registry: FunctionRegistry) -> FunctionRegistry {
        let local = registry.local_gate();
        let inner: Arc<dyn RateGate> = match &self.coordinator {
            Some(coordinator) => {
                let gate = Arc::new(CoordinatorGate::new(
                    Arc::clone(coordinator),
                    &self.site,
                    local,
                    Arc::clone(&self.clock),
                ));
                self.installed
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(Arc::clone(&gate));
                gate
            }
            None => local,
        };
        registry.with_gate(Arc::new(MeteredGate::new(inner, Arc::clone(&self.clock))))
    }

    /// How many calls fell back to the node's own bucket, over every gate installed so far.
    pub fn fallbacks(&self) -> u64 {
        self.installed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|gate| gate.fallbacks())
            .sum()
    }

    /// How many coordinator gates have been built (one per registry build in the cluster
    /// profile, none otherwise).
    pub fn gates_built(&self) -> usize {
        self.installed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

/// The coordinator the cluster profile shares limits through, or `None` under any other profile.
///
/// A coordinator a module started before this one provided as a port is used when there is one;
/// otherwise this builds the database coordinator over the site's own pool, as the events
/// service does, and prepares its tables.
///
/// # Errors
///
/// The database coordinator's tables cannot be prepared.
pub async fn coordinator_for(ctx: &SiteCtx<'_>) -> Result<Option<Arc<dyn Coordinator>>> {
    let env = *ctx.env()?;
    if env.loaded.common.services.profile != CLUSTER_PROFILE {
        return Ok(None);
    }
    if let Some(shared) = ctx.port::<dyn Coordinator>() {
        return Ok(Some(shared));
    }
    let coordinator = spot_coordinator_pg::PgCoordinator::new(
        env.backend.pool(),
        spot_coordinator_pg::PgCoordinatorConfig::default(),
    );
    coordinator
        .prepare()
        .await
        .map_err(|e| Error(format!("services.function: the coordinator: {}", e.code)))?;
    Ok(Some(Arc::new(coordinator)))
}
