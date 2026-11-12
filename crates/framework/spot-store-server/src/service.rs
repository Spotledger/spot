//! The service: the shared state every handler and worker uses, and its construction.
//!
//! [`Service`] owns the registry core, the stores behind their ports, the worker pool for
//! blocking calls, the review workers, the revocation list, the rate limits and the metrics. It
//! reads no wall clock (the injected [`Clock`] is the only source of time) and holds no key
//! other than the store key.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use serde_json::json;
use spot_bundle_signing::TrustedRoot;
use spot_registry::{
    AuditAction as RegistryAction, AuditRecord as RegistryRecord, AuditSink, BlobStore, IndexStore,
    Registry, RegistryPolicy,
};
use spot_store_state::{AuditEntry, AuditKind, StoreStateStore};

use crate::clock::Clock;
use crate::config::{Config, StartupError};
use crate::dynstore::{SharedBlobs, SharedIndex};
use crate::error::ApiError;
use crate::keys::{AdminCredential, StoreKey};
use crate::metrics::Metrics;
use crate::pool::WorkerPool;
use crate::ratelimit::Limiter;
use crate::review::{ReviewDeps, Reviewer};
use crate::revocation::Revocations;

/// How often the idempotency table is pruned, in seconds.
const PRUNE_EVERY: i64 = 3600;
/// How long an idempotency key is kept, in seconds (the contract's 24 hours).
const IDEMPOTENCY_TTL: i64 = 24 * 3600;

/// Everything a [`Service`] is built from. The binary builds the PostgreSQL stores; a test or an
/// embedder passes memory doubles.
pub struct ServiceParts {
    /// The validated configuration.
    pub config: Config,
    /// The registry catalogue.
    pub index: Arc<dyn IndexStore>,
    /// The content-addressed bundle store.
    pub blobs: Arc<dyn BlobStore>,
    /// The store's own durable state.
    pub state: Arc<dyn StoreStateStore>,
    /// The time source.
    pub clock: Arc<dyn Clock>,
    /// The one store key.
    pub store_key: StoreKey,
    /// The operator credential.
    pub admin: AdminCredential,
    /// Extra roots publisher certificates chain to (the store key is always one).
    pub trust_roots: Vec<TrustedRoot>,
}

/// The shared state behind the workers.
pub(crate) struct Core {
    pub config: Config,
    pub policy: RegistryPolicy,
    pub index: Arc<SharedIndex>,
    pub blobs: Arc<SharedBlobs>,
    pub state: Arc<dyn StoreStateStore>,
    pub clock: Arc<dyn Clock>,
    pub registry: Registry<SharedIndex, SharedBlobs>,
    pub store_key: Arc<StoreKey>,
    pub admin: AdminCredential,
    pub revocations: Revocations,
    pub reviewer: Reviewer,
    pub metrics: Arc<Metrics>,
    pub paid: BTreeSet<String>,
}

/// One structured line on stderr for an event nobody asked for (a failed audit write, a failed
/// refresh). Carries no key, token, body or credential.
pub(crate) fn log_event(event: &str, detail: &str) {
    eprintln!("{}", json!({ "event": event, "detail": detail }));
}

impl Core {
    /// Appends an audit entry. A failure is loud (a log line and a counter) but does not undo the
    /// action it records: use [`Core::audit_required`] where the entry is part of the action.
    pub fn audit(&self, entry: &AuditEntry) {
        if self.state.append_audit(entry).is_err() {
            Metrics::bump(&self.metrics.audit_failures);
            log_event("audit_failed", entry.kind.as_str());
        }
    }

    /// Appends an audit entry that must be written for the action to count.
    pub fn audit_required(&self, entry: &AuditEntry) -> Result<(), ApiError> {
        self.state.append_audit(entry)?;
        Ok(())
    }
}

/// The audit sink of the registry core: the registry's own publish record goes to the durable
/// audit log, under the actor the registry names.
struct StateAudit {
    state: Arc<dyn StoreStateStore>,
    metrics: Arc<Metrics>,
}

impl AuditSink for StateAudit {
    fn record(&self, record: RegistryRecord) {
        let kind = match record.action {
            RegistryAction::Publish => AuditKind::Publish,
            RegistryAction::Yank => AuditKind::Yank,
            RegistryAction::Withdraw => AuditKind::Withdraw,
            // The store has no transfer operation, so the registry never raises this one.
            RegistryAction::Transfer => return,
        };
        let entry = AuditEntry {
            kind,
            actor: record.who.clone(),
            publisher_id: Some(record.who),
            app: Some(record.app),
            version: record.version,
            digest: Some(record.digest),
            reason: record.reason.chars().take(512).collect(),
            at: record.at,
        };
        if self.state.append_audit(&entry).is_err() {
            Metrics::bump(&self.metrics.audit_failures);
            log_event("audit_failed", entry.kind.as_str());
        }
    }
}

/// The store server.
pub struct Service {
    pub(crate) core: Arc<Core>,
    pub(crate) pool: WorkerPool,
    pub(crate) limiter: Limiter,
    stop: AtomicBool,
    last_prune: AtomicI64,
}

impl Service {
    /// Builds the service: loads or issues the first revocation list, starts the worker pool and
    /// the review workers. Call it from a plain thread, not from inside a tokio runtime, when the
    /// stores are PostgreSQL stores (the shared bridge refuses runtime threads).
    ///
    /// # Errors
    /// `store_server.startup` when the revocation list cannot be prepared or a thread cannot be
    /// started.
    pub fn new(parts: ServiceParts) -> Result<Arc<Self>, StartupError> {
        let ServiceParts {
            config,
            index,
            blobs,
            state,
            clock,
            store_key,
            admin,
            trust_roots,
        } = parts;
        let now = clock.now();
        let metrics = Arc::new(Metrics::default());
        let store_key = Arc::new(store_key);
        let revocations = Revocations::open(
            Arc::clone(&state),
            Arc::clone(&store_key),
            trust_roots,
            config.revocation.lifetime_secs,
            config.revocation.refresh_secs,
            now,
        )?;
        let policy = RegistryPolicy::default();
        let shared_index = Arc::new(SharedIndex(index));
        let shared_blobs = Arc::new(SharedBlobs(Arc::clone(&blobs)));
        let sink: Arc<dyn AuditSink> = Arc::new(StateAudit {
            state: Arc::clone(&state),
            metrics: Arc::clone(&metrics),
        });
        let registry = Registry::new(
            Arc::clone(&shared_index),
            Arc::clone(&shared_blobs),
            policy.clone(),
            revocations.trust(),
            sink,
        );
        let thread_error =
            |what: &str| StartupError::startup(format!("could not start the {what} threads"));
        let reviewer = Reviewer::start(ReviewDeps {
            state: Arc::clone(&state),
            blobs,
            clock: Arc::clone(&clock),
            metrics: Arc::clone(&metrics),
            config: config.review.profile.clone(),
            workers: config.review.workers,
            queue: config.review.queue,
            timeout: Duration::from_secs(config.review.timeout_secs),
            max_bundle: config.limits.max_bundle_bytes,
        })
        .map_err(|_| thread_error("review"))?;
        let pool = WorkerPool::start("store-worker", config.workers.threads, config.workers.queue)
            .map_err(|_| thread_error("worker"))?;
        let limiter = Limiter::new(config.rates.clone());
        let paid = config.paid_apps.iter().cloned().collect();
        let core = Arc::new(Core {
            config,
            policy,
            index: shared_index,
            blobs: shared_blobs,
            state,
            clock,
            registry,
            store_key,
            admin,
            revocations,
            reviewer,
            metrics,
            paid,
        });
        Ok(Arc::new(Self {
            core,
            pool,
            limiter,
            stop: AtomicBool::new(false),
            last_prune: AtomicI64::new(now),
        }))
    }

    /// One round of maintenance: reissue the revocation list when it is due and prune expired
    /// idempotency keys. Runs on the calling thread, which must be a plain OS thread.
    pub fn maintain_once(&self) {
        let now = self.core.clock.now();
        if self.core.revocations.refresh_due(now)
            && let Err(error) = self.core.revocations.reissue(now)
        {
            log_event("revocation_refresh_failed", &error.code);
        }
        let last = self.last_prune.load(Ordering::Relaxed);
        if now - last >= PRUNE_EVERY {
            self.last_prune.store(now, Ordering::Relaxed);
            if self
                .core
                .state
                .idempotency_prune(now - IDEMPOTENCY_TTL)
                .is_err()
            {
                log_event("idempotency_prune_failed", "store_state");
            }
        }
    }

    /// Starts the maintenance thread: a round every 30 seconds until [`Service::shutdown`].
    ///
    /// # Errors
    /// The operating system's refusal to start a thread.
    pub fn spawn_maintenance(self: &Arc<Self>) -> std::io::Result<()> {
        let service = Arc::clone(self);
        std::thread::Builder::new()
            .name("store-maintenance".to_owned())
            .spawn(move || {
                while !service.stop.load(Ordering::SeqCst) {
                    service.maintain_once();
                    for _ in 0..30 {
                        if service.stop.load(Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            })
            .map(|_| ())
    }

    /// Asks the maintenance thread to stop.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// The generation of the revocation list being served.
    #[must_use]
    pub fn revocation_generation(&self) -> u64 {
        self.core.revocations.served().generation
    }
}
