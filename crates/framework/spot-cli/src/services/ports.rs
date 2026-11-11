//! Narrow ports over the site's job machinery, for modules that start after `jobs`.
//!
//! The `jobs` module owns the job store, the coordinator and the engine handle. A later module
//! (outbound webhooks, WP-INT-01.T2) needs three things from them and nothing more, and takes
//! them through the typed ports map (`ctx.require_port::<dyn X>()`):
//!
//! - [`JobSubmit`]: create one job with an idempotency key. No list, cancel or read of jobs.
//! - [`OutboxLease`]: hold a named consumer lease and read the outbox from a position.
//! - [`SystemReader`]: read one document as the platform itself. No write, no list.
//!
//! Every port is built for one site from that site's own store, coordinator, feed and engine
//! and carries the site id; no method takes a site id. Once the site's stop signal arrives
//! every method answers [`PortError::Stopped`] at once. An error never carries a payload, a
//! key, a document name or a secret.
//!
//! The job handler registry is not a port: a module registers its handler with
//! `ctx.job_handlers()` as mail does, and the jobs module takes the registry in its finaliser.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value as Json;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::Document;
use spot_app_api::error::ErrorClass;
use spot_app_api::prelude::{Name, RecordId};
use spot_coordinator::{Coordinator, Lease};
use spot_core::{Clock, SiteId};
use spot_jobs::JobStore;
use spot_jobs::model::{InsertResult, NewJob};
use spot_storage_contract::{ChangeFeed, FeedPosition, OutboxRecord};
use time::OffsetDateTime;
use tokio::sync::watch;

use crate::live::SiteLive;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};

/// The longest idempotency key a job may carry.
pub const MAX_KEY_BYTES: usize = 200;
/// The most serialised bytes a job's arguments may take.
pub const MAX_ARGS_BYTES: usize = 64 * 1024;
/// The most outbox events one read returns.
pub const MAX_OUTBOX_PAGE: u32 = 500;

/// Why a port refused or failed. The text never quotes a caller's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortError {
    /// The site is stopping: the port is closed.
    Stopped,
    /// The request is malformed or names something this site does not have.
    Invalid(&'static str),
    /// The lease is held by another node, or the coordinator cannot be reached.
    LeaseUnavailable,
    /// The backing service (store, feed or engine) could not answer.
    Unavailable(&'static str),
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => f.write_str("the site is stopping; the port is closed"),
            Self::Invalid(why) => write!(f, "invalid request: {why}"),
            Self::LeaseUnavailable => f.write_str("the lease is not available"),
            Self::Unavailable(what) => write!(f, "{what} is not available"),
        }
    }
}

impl std::error::Error for PortError {}

/// A port call's future.
pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, PortError>> + Send + 'a>>;

/// Closes every port of one site when the site's stop signal arrives.
#[derive(Debug, Default)]
pub struct StopGate(AtomicBool);

impl StopGate {
    /// An open gate.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Closes the gate for good.
    pub fn close(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether the gate is closed.
    pub fn is_closed(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<(), PortError> {
        if self.is_closed() {
            Err(PortError::Stopped)
        } else {
            Ok(())
        }
    }
}

/// The service that closes the gate when the stop signal arrives.
struct GateService(Arc<StopGate>);

impl Service for GateService {
    fn name(&self) -> &'static str {
        "service_ports"
    }

    fn level(&self) -> Level {
        // The ports are a convenience of later modules; the site does not depend on them.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                if *stop.borrow() {
                    break;
                }
                if stop.changed().await.is_err() {
                    break;
                }
            }
            self.0.close();
            Ok(())
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            self.0.close();
            Ok(())
        })
    }
}

// --- JobSubmit ----------------------------------------------------------------------------

/// One job to create.
#[derive(Clone, Debug)]
pub struct JobSpec {
    /// The job name, qualified by app (`erp_selling:send_reminders`).
    pub name: String,
    /// The queue; one of the queues this site's workers claim.
    pub queue: String,
    /// The arguments.
    pub args: Json,
    /// The deduplication key: the same name and key create one job.
    pub idempotency_key: String,
    /// Not before this instant; `None` means now.
    pub not_before: Option<OffsetDateTime>,
}

/// Creates jobs for this site. It cannot list, read or cancel jobs.
pub trait JobSubmit: Send + Sync {
    /// Creates the job unless one with the same name and key exists. `true` means it is new.
    fn submit<'a>(&'a self, job: JobSpec) -> PortFuture<'a, bool>;
}

/// [`JobSubmit`] over the site's store.
pub struct StoreJobSubmit {
    site: SiteId,
    store: Arc<dyn JobStore>,
    queues: Vec<String>,
    clock: Arc<dyn Clock>,
    gate: Arc<StopGate>,
}

impl StoreJobSubmit {
    /// A submit port for `site`.
    pub fn new(
        site: SiteId,
        store: Arc<dyn JobStore>,
        queues: Vec<String>,
        clock: Arc<dyn Clock>,
        gate: Arc<StopGate>,
    ) -> Self {
        Self {
            site,
            store,
            queues,
            clock,
            gate,
        }
    }
}

impl JobSubmit for StoreJobSubmit {
    fn submit<'a>(&'a self, job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async move {
            self.gate.check()?;
            if !NewJob::validate_job_name(&job.name) {
                return Err(PortError::Invalid("the job name is malformed"));
            }
            if !self.queues.contains(&job.queue) {
                return Err(PortError::Invalid("the queue is not one of this site's"));
            }
            if job.idempotency_key.is_empty() || job.idempotency_key.len() > MAX_KEY_BYTES {
                return Err(PortError::Invalid("the idempotency key has a bad length"));
            }
            let args_len = serde_json::to_vec(&job.args).map_or(usize::MAX, |b| b.len());
            if args_len > MAX_ARGS_BYTES {
                return Err(PortError::Invalid("the arguments are too large"));
            }
            let not_before = job.not_before.unwrap_or_else(|| self.clock.now_utc());
            let new = NewJob::new(job.name, job.args, job.queue, self.site.clone(), not_before)
                .with_idempotency_key(job.idempotency_key);
            match self.store.insert_if_new(new).await {
                Ok(InsertResult::Inserted(_)) => Ok(true),
                Ok(InsertResult::Duplicate(_)) => Ok(false),
                Err(_) => Err(PortError::Unavailable("the job store")),
            }
        })
    }
}

// --- OutboxLease --------------------------------------------------------------------------

/// A held consumer lease. Only [`OutboxLease::acquire`] makes one.
#[derive(Clone, Debug)]
pub struct ConsumerLease(Lease);

impl ConsumerLease {
    /// The consumer's name.
    pub fn consumer(&self) -> &str {
        self.0
            .name
            .rsplit_once(':')
            .map_or(self.0.name.as_str(), |(_, c)| c)
    }
}

/// Holds one named consumer lease of this site and reads the site's outbox.
///
/// The caller keeps its own position (a [`FeedPosition`] it stored) and passes it to
/// [`read`](Self::read); it must check [`still_held`](Self::still_held) before it acts on a
/// page, as the jobs intake does.
pub trait OutboxLease: Send + Sync {
    /// Takes the consumer's lease. [`PortError::LeaseUnavailable`] means another node holds it.
    fn acquire<'a>(&'a self, consumer: &'a str) -> PortFuture<'a, ConsumerLease>;
    /// Renews the lease; the returned handle replaces the old one.
    fn renew<'a>(&'a self, lease: &'a ConsumerLease) -> PortFuture<'a, ConsumerLease>;
    /// Whether the lease's fencing token is still the current one.
    fn still_held<'a>(&'a self, lease: &'a ConsumerLease) -> PortFuture<'a, bool>;
    /// Gives the lease up at once.
    fn release<'a>(&'a self, lease: &'a ConsumerLease) -> PortFuture<'a, ()>;
    /// Up to `limit` (at most [`MAX_OUTBOX_PAGE`]) published outbox events after `after`.
    fn read<'a>(
        &'a self,
        after: FeedPosition,
        limit: u32,
    ) -> PortFuture<'a, Vec<OutboxRecord>>;
}

/// [`OutboxLease`] over the site's coordinator and change feed.
pub struct CoordinatedOutbox<F: ChangeFeed> {
    site: SiteId,
    coordinator: Arc<dyn Coordinator>,
    feed: F,
    holder: String,
    ttl: Duration,
    gate: Arc<StopGate>,
}

impl<F: ChangeFeed> CoordinatedOutbox<F> {
    /// An outbox port for `site`, holding leases as `holder` for `ttl`.
    pub fn new(
        site: SiteId,
        coordinator: Arc<dyn Coordinator>,
        feed: F,
        holder: String,
        ttl: Duration,
        gate: Arc<StopGate>,
    ) -> Self {
        Self {
            site,
            coordinator,
            feed,
            holder,
            ttl,
            gate,
        }
    }

    fn prefix(&self) -> String {
        format!("outbox:{}:", self.site)
    }

    /// The lease must be one this port made, for this site.
    fn own(&self, lease: &ConsumerLease) -> Result<(), PortError> {
        if lease.0.name.starts_with(&self.prefix()) {
            Ok(())
        } else {
            Err(PortError::Invalid("the lease is not this site's"))
        }
    }
}

fn consumer_name(consumer: &str) -> bool {
    !consumer.is_empty()
        && consumer.len() <= 64
        && consumer
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl<F: ChangeFeed> OutboxLease for CoordinatedOutbox<F> {
    fn acquire<'a>(&'a self, consumer: &'a str) -> PortFuture<'a, ConsumerLease> {
        Box::pin(async move {
            self.gate.check()?;
            if !consumer_name(consumer) {
                return Err(PortError::Invalid("the consumer name is malformed"));
            }
            let name = format!("{}{consumer}", self.prefix());
            self.coordinator
                .acquire(&name, &self.holder, self.ttl)
                .await
                .map(ConsumerLease)
                .map_err(|_| PortError::LeaseUnavailable)
        })
    }

    fn renew<'a>(&'a self, lease: &'a ConsumerLease) -> PortFuture<'a, ConsumerLease> {
        Box::pin(async move {
            self.gate.check()?;
            self.own(lease)?;
            self.coordinator
                .renew(&lease.0, self.ttl)
                .await
                .map(ConsumerLease)
                .map_err(|_| PortError::LeaseUnavailable)
        })
    }

    fn still_held<'a>(&'a self, lease: &'a ConsumerLease) -> PortFuture<'a, bool> {
        Box::pin(async move {
            self.gate.check()?;
            self.own(lease)?;
            Ok(self
                .coordinator
                .check_fence(&lease.0.name, lease.0.token)
                .await
                .is_ok())
        })
    }

    fn release<'a>(&'a self, lease: &'a ConsumerLease) -> PortFuture<'a, ()> {
        Box::pin(async move {
            // A closed gate still lets a holder give its lease up: that is the polite exit.
            self.own(lease)?;
            let _ = self.coordinator.release(&lease.0).await;
            Ok(())
        })
    }

    fn read<'a>(
        &'a self,
        after: FeedPosition,
        limit: u32,
    ) -> PortFuture<'a, Vec<OutboxRecord>> {
        Box::pin(async move {
            self.gate.check()?;
            self.feed
                .read_outbox(after, limit.clamp(1, MAX_OUTBOX_PAGE))
                .await
                .map_err(|_| PortError::Unavailable("the outbox"))
        })
    }
}

// --- SystemReader -------------------------------------------------------------------------

/// Reads one document of this site as the platform itself. It cannot write or list.
pub trait SystemReader: Send + Sync {
    /// The document of `doctype` named `name` (its record id), or `None` when this site has
    /// none. Never another site's.
    fn read<'a>(&'a self, doctype: &'a str, name: &'a str) -> PortFuture<'a, Option<Document>>;
}

/// Where a reader finds the engine to load through. The live hub gives the running one; a test
/// gives a fake.
pub trait DocumentLoader: Send + Sync {
    /// Loads one document for `inv` (whose site is the reader's), or `Ok(None)` when absent.
    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> PortFuture<'a, Option<Document>>;
}

/// The loader over the engine the site's live hub serves now (it exists once the runtime is up).
pub struct LiveLoader(pub Arc<SiteLive>);

impl DocumentLoader for LiveLoader {
    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> PortFuture<'a, Option<Document>> {
        Box::pin(async move {
            let engine = self
                .0
                .engine()
                .ok_or(PortError::Unavailable("the document engine"))?;
            match engine.load(inv, doctype, id).await {
                Ok(loaded) => Ok(Some(loaded.value)),
                Err(e) if e.class == ErrorClass::NotFound => Ok(None),
                Err(_) => Err(PortError::Unavailable("the document engine")),
            }
        })
    }
}

/// [`SystemReader`] for one site.
pub struct EngineReader {
    site: SiteId,
    loader: Arc<dyn DocumentLoader>,
    clock: Arc<dyn Clock>,
    gate: Arc<StopGate>,
}

impl EngineReader {
    /// A reader for `site`.
    pub fn new(
        site: SiteId,
        loader: Arc<dyn DocumentLoader>,
        clock: Arc<dyn Clock>,
        gate: Arc<StopGate>,
    ) -> Self {
        Self {
            site,
            loader,
            clock,
            gate,
        }
    }
}

impl SystemReader for EngineReader {
    fn read<'a>(&'a self, doctype: &'a str, name: &'a str) -> PortFuture<'a, Option<Document>> {
        Box::pin(async move {
            self.gate.check()?;
            let id = spot_http::api::codec::parse_record_id(name)
                .ok_or(PortError::Invalid("the document name is not a record id"))?;
            let component = Name::new("service_ports")
                .map_err(|_| PortError::Unavailable("the reader identity"))?;
            let inv = Invocation {
                site: self.site.clone(),
                actor: Actor::System { component },
                command_id: None,
                language: None,
                now: self.clock.now_utc(),
            };
            self.loader.load(&inv, doctype, id).await
        })
    }
}

// --- providing ----------------------------------------------------------------------------

/// What the ports are built from: the site's own parts.
pub struct PortParts<F: ChangeFeed + 'static> {
    /// The site.
    pub site: SiteId,
    /// The site's job store.
    pub store: Arc<dyn JobStore>,
    /// The queues the site's workers claim.
    pub queues: Vec<String>,
    /// The site clock.
    pub clock: Arc<dyn Clock>,
    /// The coordinator the site's leases live in.
    pub coordinator: Arc<dyn Coordinator>,
    /// The site's change feed (its own database).
    pub feed: F,
    /// This node's identity in leases.
    pub node: String,
    /// The consumer lease's time to live.
    pub ttl: Duration,
    /// Where documents are loaded through.
    pub loader: Arc<dyn DocumentLoader>,
}

/// Provides [`JobSubmit`], [`OutboxLease`] and [`SystemReader`] to the modules that start after
/// the current one, and adds the service that closes them at the stop signal.
///
/// Called on the assembling build only: a reload keeps the running services and has no store
/// to build ports from, so a module that needs a port builds its service only when
/// `ctx.assembling()` is true.
pub fn provide<F: ChangeFeed + 'static>(
    ctx: &mut SiteCtx<'_>,
    parts: PortParts<F>,
) -> crate::Result<()> {
    let gate = StopGate::new();
    let submit: Arc<dyn JobSubmit> = Arc::new(StoreJobSubmit::new(
        parts.site.clone(),
        parts.store,
        parts.queues,
        Arc::clone(&parts.clock),
        Arc::clone(&gate),
    ));
    let outbox: Arc<dyn OutboxLease> = Arc::new(CoordinatedOutbox::new(
        parts.site.clone(),
        parts.coordinator,
        parts.feed,
        parts.node,
        parts.ttl,
        Arc::clone(&gate),
    ));
    let reader: Arc<dyn SystemReader> = Arc::new(EngineReader::new(
        parts.site,
        parts.loader,
        parts.clock,
        Arc::clone(&gate),
    ));
    ctx.provide(submit)?;
    ctx.provide(outbox)?;
    ctx.provide(reader)?;
    ctx.add(Arc::new(GateService(gate)));
    Ok(())
}
