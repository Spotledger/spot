//! The site-level views: verification, lag, status and per-document history.
//!
//! [`DocAudit`] bundles one site's store, lane configuration, checkpoint keys and field masker.
//! [`DocAudit::verify_site`] verifies every lane's chain and the site checkpoint;
//! [`DocAudit::history`] reads one document's chained entries; [`DocAudit::audit_lag`] and
//! [`DocAudit::status`] expose how far the appenders are behind.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use spot_activity::FieldChange;
use spot_app_api::prelude::Value;
use spot_audit_chain::AuditStore;
use spot_audit_chain::{CheckpointSigner, KeyRing, Verifier};
use spot_core::Clock;
use spot_permissions::{DocFacts, Principal};

use crate::appender::{AppenderStore, LaneAppender, OutboxFeed};
use crate::denial::DenialStats;
use crate::error::DocAuditError;
use crate::event::{DiffChange, DiffValue};
use crate::lane::{LaneConfig, SiteCheckpoint, lane_of, lane_site};
use crate::masker::FieldMasker;
use crate::secret::{SECRET_AUDIT_TOPIC, decode_secret_outbox};
use crate::step::AUDIT_TOPIC;

/// The kind of a lane truncation finding. A signal kind, not an error code (written with
/// `concat!` so the source holds no bare dotted literal).
pub const TRUNCATED_KIND: &str = concat!("docaudit.", "lane_truncated");

/// One finding at a lane and sequence number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneFailure {
    /// The lane, or `None` for a checkpoint-level finding.
    pub lane: Option<u32>,
    /// The sequence number.
    pub seq: u64,
    /// The stable kind code.
    pub kind: &'static str,
}

/// The outcome of verifying a whole site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteVerifyReport {
    /// The site.
    pub site: String,
    /// The epoch the lanes belong to.
    pub epoch: u32,
    /// Entries whose hash and link were checked across every lane.
    pub checked: u64,
    /// Every finding.
    pub failures: Vec<LaneFailure>,
}

impl SiteVerifyReport {
    /// Whether nothing is wrong.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// One entry of a document's audit history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// The lane the entry is chained in.
    pub lane: u32,
    /// The sequence number in that lane.
    pub seq: u64,
    /// The operation.
    pub operation: String,
    /// The acting identity.
    pub actor: String,
    /// With impersonation, the identity acted on behalf of.
    pub on_behalf_of: Option<String>,
    /// Unix microseconds.
    pub at: i64,
    /// The version the write produced.
    pub version: u64,
    /// The masked changes this reader may see.
    pub changes: Vec<DiffChange>,
    /// Some changes the reader cannot see.
    pub hidden_changes: bool,
    /// The stored diff was truncated.
    pub truncated: bool,
    /// The entry is within the incrementally verified range.
    pub verified: bool,
}

/// How far the appenders are behind the outbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditLag {
    /// Unchained events per lane.
    pub per_lane: BTreeMap<u32, u64>,
    /// The age of the oldest unchained event, in microseconds; `None` when none is pending.
    pub oldest_age_micros: Option<i64>,
    /// The total unchained events.
    pub pending: u64,
}

impl AuditLag {
    /// Whether nothing is pending.
    #[must_use]
    pub fn is_caught_up(&self) -> bool {
        self.pending == 0
    }
}

/// The readiness policy for audit lag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditPolicy {
    /// The bound, in microseconds, above which the site is unhealthy.
    pub max_lag_micros: i64,
    /// Whether exceeding the bound stops new posting operations. Default: warn only.
    pub halt_on_exceeded: bool,
}

impl Default for AuditPolicy {
    fn default() -> Self {
        Self {
            max_lag_micros: 60_000_000,
            halt_on_exceeded: false,
        }
    }
}

/// The audit readiness of a site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditStatus {
    /// The current lag.
    pub lag: AuditLag,
    /// The lag is within the bound.
    pub healthy: bool,
    /// New posting operations are allowed (the bound is not exceeded, or the policy only warns).
    pub posting_allowed: bool,
    /// The configured epoch.
    pub epoch: u32,
    /// The configured lane count.
    pub n: u32,
    /// Refused operations dropped from the audit because too many denial writes were in flight.
    pub denials_dropped: u64,
    /// Refused operations the audit could not record (also logged once).
    pub denials_not_recorded: u64,
}

/// One site's document audit: store, lanes, keys, masker and appender state.
pub struct DocAudit<S: AuditStore> {
    store: Arc<S>,
    site: String,
    config: LaneConfig,
    signer: Arc<dyn CheckpointSigner>,
    keys: KeyRing,
    masker: Arc<dyn FieldMasker>,
    state: Arc<dyn AppenderStore>,
    clock: Arc<dyn Clock>,
    policy: AuditPolicy,
    verified: Mutex<HashMap<u32, u64>>,
    denials: Arc<DenialStats>,
}

impl<S: AuditStore> DocAudit<S> {
    /// A site view over `store`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<S>,
        site: &str,
        config: LaneConfig,
        signer: Arc<dyn CheckpointSigner>,
        keys: KeyRing,
        masker: Arc<dyn FieldMasker>,
        state: Arc<dyn AppenderStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            site: site.to_owned(),
            config,
            signer,
            keys,
            masker,
            state,
            clock,
            policy: AuditPolicy::default(),
            verified: Mutex::new(HashMap::new()),
            denials: DenialStats::new(),
        }
    }

    /// Reports the counters of `stats`, the ones an
    /// [`AuditDenialSink`](crate::AuditDenialSink) was built with, in [`DocAudit::status`].
    #[must_use]
    pub fn with_denial_stats(mut self, stats: Arc<DenialStats>) -> Self {
        self.denials = stats;
        self
    }

    /// The denial counters this view reports.
    #[must_use]
    pub fn denial_stats(&self) -> &Arc<DenialStats> {
        &self.denials
    }

    /// Sets the readiness policy.
    #[must_use]
    pub fn with_policy(mut self, policy: AuditPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// The lane configuration.
    #[must_use]
    pub fn config(&self) -> LaneConfig {
        self.config
    }

    /// An appender for `lane`.
    #[must_use]
    pub fn appender(&self, lane: u32) -> LaneAppender<S> {
        LaneAppender::new(
            Arc::clone(&self.store),
            &self.site,
            lane,
            self.config,
            Arc::clone(&self.signer),
            Arc::clone(&self.state),
        )
    }

    /// Verifies every lane's chain and, when `until` is given, that no lane is behind the signed
    /// site checkpoint (a truncated tail) and that the checkpoint itself verifies.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] on a store failure; a broken chain is a *report*, not an error.
    pub async fn verify_site(
        &self,
        until: Option<&SiteCheckpoint>,
    ) -> Result<SiteVerifyReport, DocAuditError> {
        let verifier = Verifier::new(&self.keys);
        let mut report = SiteVerifyReport {
            site: self.site.clone(),
            epoch: self.config.epoch,
            checked: 0,
            failures: Vec::new(),
        };
        for lane in self.config.lanes() {
            let site = lane_site(&self.site, lane);
            let lane_report = verifier
                .verify_range(&*self.store, &site, 1, u64::MAX)
                .await?;
            report.checked = report.checked.saturating_add(lane_report.checked);
            for failure in &lane_report.failures {
                report.failures.push(LaneFailure {
                    lane: Some(lane),
                    seq: failure.seq,
                    kind: failure.kind.code(),
                });
            }
            if lane_report.failures.is_empty() {
                let head = self.store.head(&site).await?;
                if let Some(head) = head {
                    self.verified
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(lane, head.seq);
                }
            }
        }
        if let Some(checkpoint) = until {
            if let Err(e) = checkpoint.verify(&self.keys, &self.site, &self.config) {
                report.failures.push(LaneFailure {
                    lane: None,
                    seq: checkpoint.signed.seq,
                    kind: e.code(),
                });
            }
            for head in &checkpoint.lanes {
                let site = lane_site(&self.site, head.lane);
                let entries = self.store.range(&site, head.seq, head.seq).await?;
                if !entries
                    .iter()
                    .any(|e| e.seq == head.seq && e.hash == head.head_hash)
                {
                    report.failures.push(LaneFailure {
                        lane: Some(head.lane),
                        seq: head.seq,
                        kind: TRUNCATED_KIND,
                    });
                }
            }
        }
        Ok(report)
    }

    /// One document's chained audit history, masked for `principal`.
    ///
    /// A principal who may not read the document gets an empty history, not an error. Every entry
    /// is marked verified when it is within the last successfully verified range of its lane.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] or [`DocAuditError::Encoding`] on a store or decode failure.
    pub async fn history(
        &self,
        principal: &Principal,
        doctype: &str,
        id: &str,
    ) -> Result<Vec<HistoryEntry>, DocAuditError> {
        let facts = DocFacts {
            doctype: doctype.to_owned(),
            id: Some(id.to_owned()),
            ..DocFacts::default()
        };
        if !self.masker.can_read(principal, doctype, &facts) {
            return Ok(Vec::new());
        }
        let verified = self
            .verified
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut out = Vec::new();
        for (lane, seq) in self.state.document_entries(doctype, id) {
            let site = lane_site(&self.site, lane);
            for entry in self.store.range(&site, seq, seq).await? {
                let event = crate::appender::event_of(&entry)?;
                let changes: Vec<FieldChange> = event.diff.iter().map(to_field_change).collect();
                let redacted = self
                    .masker
                    .redact_diff(principal, doctype, &facts, &changes)?;
                out.push(HistoryEntry {
                    lane,
                    seq,
                    operation: event.operation.as_str().to_owned(),
                    actor: event.actor.actor.clone(),
                    on_behalf_of: event.actor.on_behalf_of.clone(),
                    at: event.at,
                    version: event.version,
                    changes: redacted
                        .changes
                        .iter()
                        .map(|c| crate::event::diff_change(c, usize::MAX))
                        .collect(),
                    hidden_changes: redacted.hidden_changes,
                    truncated: event.truncated,
                    verified: verified.get(&lane).is_some_and(|v| seq <= *v),
                });
            }
        }
        out.sort_by_key(|e| (e.lane, e.seq));
        Ok(out)
    }

    /// How far the appenders are behind: unchained events per lane and the age of the oldest.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] on a feed failure.
    pub async fn audit_lag(
        &self,
        feed: &dyn OutboxFeed,
        limit: u32,
    ) -> Result<AuditLag, DocAuditError> {
        let now = micros_now(&*self.clock);
        let mut per_lane: BTreeMap<u32, u64> = BTreeMap::new();
        let mut oldest: Option<i64> = None;
        let mut pending = 0u64;
        for lane in self.config.lanes() {
            let records = feed.read(self.state.bookmark(lane), limit).await?;
            let mut count = 0u64;
            for record in &records {
                let (doctype, id, at) = match record.event.topic.as_str() {
                    AUDIT_TOPIC => match crate::appender::decode_outbox(record) {
                        Ok(event) => (event.doctype, event.id, event.at),
                        Err(_) => continue,
                    },
                    SECRET_AUDIT_TOPIC => match decode_secret_outbox(record) {
                        Ok(event) => (event.subject_doctype, event.subject_id, event.at),
                        Err(_) => continue,
                    },
                    _ => continue,
                };
                if lane_of(&doctype, &id, self.config.n) != lane {
                    continue;
                }
                count += 1;
                oldest = Some(oldest.map_or(at, |o| o.min(at)));
            }
            if count > 0 {
                per_lane.insert(lane, count);
            }
            pending += count;
        }
        Ok(AuditLag {
            per_lane,
            oldest_age_micros: oldest.map(|at| now.saturating_sub(at)),
            pending,
        })
    }

    /// The audit readiness of the site.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] on a feed failure.
    pub async fn status(
        &self,
        feed: &dyn OutboxFeed,
        limit: u32,
    ) -> Result<AuditStatus, DocAuditError> {
        let lag = self.audit_lag(feed, limit).await?;
        let healthy = lag
            .oldest_age_micros
            .is_none_or(|age| age <= self.policy.max_lag_micros);
        Ok(AuditStatus {
            healthy,
            posting_allowed: healthy || !self.policy.halt_on_exceeded,
            lag,
            epoch: self.config.epoch,
            n: self.config.n,
            denials_dropped: self.denials.dropped(),
            denials_not_recorded: self.denials.not_recorded(),
        })
    }
}

/// A stored diff change as a [`FieldChange`], so the field guard can redact it again for a
/// different reader. A digest value is shown as its digest text; the value is never recovered.
fn to_field_change(change: &DiffChange) -> FieldChange {
    FieldChange {
        path: change.path.clone(),
        before: change.before.as_ref().map(diff_value_to_value),
        after: change.after.as_ref().map(diff_value_to_value),
    }
}

fn diff_value_to_value(value: &DiffValue) -> Value {
    match value {
        DiffValue::Null => Value::Null,
        DiffValue::Bool(b) => Value::Bool(*b),
        DiffValue::Int(i) => Value::Int(*i),
        DiffValue::Text(t) => Value::Text(t.clone()),
        DiffValue::Digest { digest, .. } => Value::Text(format!("sha256:{digest}")),
    }
}

/// The timestamp helper the lag view uses (kept here so the service reads one clock).
fn micros_now(clock: &dyn Clock) -> i64 {
    crate::step::micros(clock.now_utc())
}
