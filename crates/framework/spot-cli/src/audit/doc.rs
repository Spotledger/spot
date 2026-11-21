//! The document audit, hosted (WP-PRD-01.T22, ADR-061, ADR-070 s9).
//!
//! `spot-doc-audit` owns the audit participant, the denial sink and the lane appenders;
//! `spot-audit-store` owns the PostgreSQL chain and appender state. This module is the one place
//! that assembles them for a site:
//!
//! - [`masker_for`]: a [`SiteMasker`] over **one metadata generation and its permission
//!   decision**. It is built once per `build_site` and once per `SiteLive::swap_to`, so the mask
//!   follows the metadata. A construction that fails fails the site closed.
//! - [`DocAuditHost`]: the lane appenders (one per lane over [`PgAuditStore`] and
//!   [`PgAppenderStore`]), the denial sink over [`PgDenialWriter`], the shared denial counters
//!   and the readiness status.
//! - [`DocAuditService`]: a required service that every poll asks the job queue for one
//!   chain-append job per lane. The job name and the idempotency key (lane and time slot) make a
//!   restart, or a second node, ask for the same job and get one: the queue deduplicates, the
//!   appender's event index makes a repeated drain chain nothing twice.
//!
//! # What is never written
//!
//! No event body, actor or document value reaches a log line, an error or the readiness text:
//! the status carries counts and ages only, and every error here is a fixed sentence plus the
//! stable code of the failing layer.
//!
//! # Checkpoint signing
//!
//! The host holds no checkpoint signing key yet, so the appenders are built with a signer that
//! refuses ([`NoSigner`]): entries are chained and hash-linked, a signed site checkpoint is not
//! written. This is the same stance as the identity chain (`audit::chain`); a key custody
//! decision is an open question of the task.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use spot_app_api::prelude::Clock;
use spot_audit_chain::{AuditError as ChainError, CheckpointSigner, KeyRing};
use spot_audit_store::{PgAppenderStore, PgAuditStore, PgDenialWriter};
use spot_core::SiteId;
use spot_doc_audit::{
    AuditDenialSink, DenialStats, DocAudit, FieldMasker, LaneConfig, OutboxFeed, append_job_name,
    register_appender,
};
use spot_field_guard::{AuditRecord as GuardAudit, AuditSink, FieldAccessRecord, Offer, TraceSink};
use spot_meta::EffectiveMeta;
use spot_permissions::PermissionEngine;
use spot_postgres::PgBackend;
use spot_site_masker::{GuardDeps, SiteMasker};
use spot_storage_contract::Catalog;
use tokio::sync::watch;

use crate::config;
use crate::services::ports::{JobSpec, JobSubmit};
use crate::services::{CheckFuture, Level, RunFuture, Service};
use crate::{Error, Result};

/// Refused at start: the masker of a metadata generation could not be built.
pub const MASKER_UNAVAILABLE: &str = "cli.doc_audit_masker_unavailable";
/// Refused at start: the lane setting is out of range.
pub const LANES_INVALID: &str = "cli.doc_audit_lanes_invalid";
/// Refused at start: the audit state cannot be read.
pub const STATE_UNAVAILABLE: &str = "cli.doc_audit_state_unavailable";

/// How many outbox events the readiness lag scan looks at, at most.
const LAG_SCAN: u32 = 1_000;

/// A signer that never signs: automatic checkpoints are refused, the append itself is kept
/// (`AuditChain::append` leaves the entry appended when the signer errors).
struct NoSigner;

impl CheckpointSigner for NoSigner {
    fn key_id(&self) -> String {
        String::new()
    }
    fn sign(&self, _message: &[u8]) -> std::result::Result<Vec<u8>, ChainError> {
        Err(ChainError::Signer("no checkpoint key is configured".into()))
    }
}

/// The field guard's sinks: a record names who, what and through which door, never a value.
struct GuardSinks;

impl TraceSink for GuardSinks {
    fn offer(&self, record: FieldAccessRecord) -> Offer {
        tracing::debug!(
            target: "spot::audit::field_access",
            doctype = %record.doctype,
            field = %record.field,
            authorised = record.authorised,
            "field access (audit masking)"
        );
        Offer::Accepted
    }
}

impl AuditSink for GuardSinks {
    fn offer(&self, _record: GuardAudit) -> Offer {
        Offer::Accepted
    }
}

/// The masker of one metadata generation, fail-closed (an unknown DocType masks everything).
///
/// # Errors
///
/// `cli.doc_audit_masker_unavailable` when the guard cannot be built; the message holds the code
/// only.
pub fn masker_for(
    meta: &Arc<EffectiveMeta>,
    decision: &Arc<PermissionEngine>,
    clock: &Arc<dyn Clock>,
) -> Result<Arc<dyn FieldMasker>> {
    let sinks = Arc::new(GuardSinks);
    let masker = SiteMasker::new(
        meta,
        GuardDeps {
            engine: Arc::clone(decision),
            trace: sinks.clone(),
            audit: sinks,
            clock: Arc::clone(clock),
        },
    );
    Ok(Arc::new(masker))
}

/// The lane configuration of a site, or the refusal.
///
/// # Errors
///
/// `cli.doc_audit_lanes_invalid` when a setting is out of range.
pub fn lane_config(audit: &config::Audit) -> Result<LaneConfig> {
    if !(1..=64).contains(&audit.doc_lanes) {
        return Err(Error(format!(
            "{LANES_INVALID}: `[audit] doc_lanes` is {}, not 1 to 64",
            audit.doc_lanes
        )));
    }
    if !(100..=60_000).contains(&audit.doc_append_poll_ms) {
        return Err(Error(format!(
            "{LANES_INVALID}: `[audit] doc_append_poll_ms` is {}, not 100 to 60000",
            audit.doc_append_poll_ms
        )));
    }
    if !(1..=5_000).contains(&audit.doc_append_batch) {
        return Err(Error(format!(
            "{LANES_INVALID}: `[audit] doc_append_batch` is {}, not 1 to 5000",
            audit.doc_append_batch
        )));
    }
    Ok(LaneConfig::new(audit.doc_lanes, audit.doc_lane_epoch))
}

/// What readiness asks the audit for.
pub trait AuditReport: Send + Sync {
    /// The one-line status, or the reason it cannot be read. Counts and ages only.
    fn report(&self) -> Pin<Box<dyn Future<Output = String> + Send + '_>>;
}

/// One site's hosted document audit.
pub struct DocAuditHost {
    audit: Arc<DocAudit<PgAuditStore>>,
    store: Arc<PgAuditStore>,
    appenders: Vec<Arc<spot_doc_audit::appender::LaneAppender<PgAuditStore>>>,
    feed: Arc<dyn OutboxFeed>,
    denial_writer: Arc<PgDenialWriter<PgBackend>>,
    stats: Arc<DenialStats>,
    lanes: LaneConfig,
    poll: Duration,
    batch: u32,
}

impl DocAuditHost {
    /// Builds the host of `site` over its backend.
    ///
    /// `masker` is the masker of the generation the site starts with; the appenders do not use it
    /// (a drain copies stored, already masked events), only the history readers do.
    ///
    /// # Errors
    ///
    /// `cli.doc_audit_lanes_invalid` for a setting out of range.
    pub fn new(
        site: &SiteId,
        backend: &PgBackend,
        catalog: Arc<Catalog>,
        clock: Arc<dyn Clock>,
        audit_config: &config::Audit,
        masker: Arc<dyn FieldMasker>,
    ) -> Result<Arc<Self>> {
        let lanes = lane_config(audit_config)?;
        let pool = backend.pool();
        let store = Arc::new(PgAuditStore::new(pool.clone()));
        let state: Arc<dyn spot_doc_audit::appender::AppenderStore> =
            Arc::new(PgAppenderStore::new(pool, site.as_str()));
        let signer: Arc<dyn CheckpointSigner> = Arc::new(NoSigner);
        let stats = DenialStats::new();
        let audit = Arc::new(
            DocAudit::new(
                Arc::clone(&store),
                site.as_str(),
                lanes,
                Arc::clone(&signer),
                KeyRing::new(),
                masker,
                Arc::clone(&state),
                clock,
            )
            .with_denial_stats(Arc::clone(&stats)),
        );
        let appenders = lanes
            .lanes()
            .map(|lane| {
                Arc::new(spot_doc_audit::appender::LaneAppender::new(
                    Arc::clone(&store),
                    site.as_str(),
                    lane,
                    lanes,
                    Arc::clone(&signer),
                    Arc::clone(&state),
                ))
            })
            .collect();
        let feed: Arc<dyn OutboxFeed> = Arc::new(backend.clone());
        let denial_writer = Arc::new(PgDenialWriter::new(
            site.as_str(),
            Arc::new(backend.clone()),
            catalog,
        ));
        Ok(Arc::new(Self {
            audit,
            store,
            appenders,
            feed,
            denial_writer,
            stats,
            lanes,
            poll: Duration::from_millis(audit_config.doc_append_poll_ms),
            batch: audit_config.doc_append_batch,
        }))
    }

    /// The denial sink of this site: one refused operation, one `denied` event after the rollback,
    /// written by [`PgDenialWriter`] with the shared counters. `node` is the partition node when
    /// the site has one.
    #[must_use]
    pub fn denial_sink(&self, node: Option<&str>) -> Arc<AuditDenialSink> {
        let sink = AuditDenialSink::with_denial_writer(
            self.denial_writer.clone(),
            Arc::clone(&self.stats),
        );
        Arc::new(match node {
            Some(node) => sink.with_node(node),
            None => sink,
        })
    }

    /// The lane count and epoch (fixed for the life of the site).
    #[must_use]
    pub fn lanes(&self) -> LaneConfig {
        self.lanes
    }

    /// Registers one append handler per lane on the job worker's registry.
    pub fn register_handlers(&self, registry: &mut spot_jobs::worker::JobHandlerRegistry) {
        for appender in &self.appenders {
            register_appender(
                registry,
                Arc::clone(appender),
                Arc::clone(&self.feed),
                self.batch,
            );
        }
    }

    /// The start-up check: the appender state and the feed can be read now. A failure is the
    /// stable code only.
    ///
    /// # Errors
    ///
    /// `cli.doc_audit_state_unavailable`.
    pub async fn check(&self) -> std::result::Result<(), String> {
        self.audit
            .status(self.feed.as_ref(), 1)
            .await
            .map(|_| ())
            .map_err(|e| format!("{STATE_UNAVAILABLE}: {}", e.code()))
    }

    /// One job per lane for the time slot `slot`.
    #[must_use]
    pub fn lane_jobs(&self, queue: &str, slot: u64) -> Vec<JobSpec> {
        self.lanes
            .lanes()
            .map(|lane| JobSpec {
                name: append_job_name(lane),
                queue: queue.to_owned(),
                args: json!({ "lane": lane }),
                idempotency_key: format!("doc_audit:{}:{}:{slot}", self.lanes.epoch, lane),
                not_before: None,
            })
            .collect()
    }

    /// The readiness line: `docaudit: on`, the lag and the denial counters. A lag over the
    /// bound is reported as `lagging`, never as a failure (warn only).
    pub async fn status_line(&self) -> String {
        match self.audit.status(self.feed.as_ref(), LAG_SCAN).await {
            Ok(status) => {
                let per_lane: BTreeMap<u32, u64> = status.lag.per_lane.clone();
                let lanes = per_lane.len().max(status.n as usize);
                format!(
                    "docaudit: on{}; {} lane(s), pending {}, oldest {} s; denials dropped {}, not recorded {}",
                    if status.healthy { "" } else { " (lagging)" },
                    lanes,
                    status.lag.pending,
                    status.lag.oldest_age_micros.unwrap_or(0).max(0) / 1_000_000,
                    status.denials_dropped,
                    status.denials_not_recorded,
                )
            }
            Err(e) => format!("docaudit: on (status unavailable: {})", e.code()),
        }
    }

    /// The chain store (for tests and the verifier).
    #[must_use]
    pub fn store(&self) -> &Arc<PgAuditStore> {
        &self.store
    }
}

impl AuditReport for DocAuditHost {
    fn report(&self) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        Box::pin(self.status_line())
    }
}

/// The required service that keeps one chain-append job per lane queued.
pub struct DocAuditService {
    host: Arc<DocAuditHost>,
    submit: Arc<dyn JobSubmit>,
    queue: String,
    clock: Arc<dyn Clock>,
}

impl DocAuditService {
    /// A service over `host`, submitting to `queue`.
    #[must_use]
    pub fn new(
        host: Arc<DocAuditHost>,
        submit: Arc<dyn JobSubmit>,
        queue: String,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        Arc::new(Self {
            host,
            submit,
            queue,
            clock,
        })
    }

    async fn tick(&self, slot: u64) {
        for job in self.host.lane_jobs(&self.queue, slot) {
            // A refused submit (the site is stopping, the queue is unknown) is retried on the next
            // tick; the appender is idempotent, so a skipped slot loses nothing.
            let _ = self.submit.submit(job).await;
        }
    }
}

impl Service for DocAuditService {
    fn name(&self) -> &'static str {
        "docaudit"
    }

    fn level(&self) -> Level {
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(self.host.check())
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let poll = self.host.poll;
            let mut tick = tokio::time::interval(poll);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    _ = tick.tick() => {
                        // The slot is wall-clock time divided by the poll: two nodes and a
                        // restart inside one poll ask for the same slot, which the queue's key
                        // dedups; a later slot is always a new key.
                        let millis = self.clock.now_utc().unix_timestamp_nanos() / 1_000_000;
                        let slot = millis / i128::try_from(poll.as_millis().max(1)).unwrap_or(1);
                        self.tick(u64::try_from(slot).unwrap_or(0)).await;
                    }
                }
            }
        })
    }
}
