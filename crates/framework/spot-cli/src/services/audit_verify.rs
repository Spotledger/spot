//! The audit chain, verified on a server (WP-SEC-06.T8; ADR-061, ADR-070).
//!
//! `spot-audit-chain` verifies a hash chain and `spot-audit-store` stores it; this module is the
//! thing that actually runs the verification and tells someone. It is the **watchman**: it never
//! fixes, rewrites or skips, and a verification failure is an alert, never only a log line.
//!
//! # What runs
//!
//! * **Hourly, incremental** (`services.audit_verify.incremental_seconds`): every lane is verified
//!   from the last verified head, then the signed head anchor is compared with the stored heads and
//!   the appender's event-id index with the chain (exactly once, WP-SEC-06.T7).
//! * **Nightly, full, resumable** (`full_seconds`): the whole chain from entry 1, in slices of
//!   `full_slice_entries`; the cursor (lane, next entry) is saved after every slice, so a restart
//!   continues where the pass stopped. A cycle that ends clean is the only thing that clears a
//!   degraded state.
//! * **Anchors** (`anchor_seconds`): the signed head of every lane (a `spot_doc_audit::SiteCheckpoint`)
//!   is appended as one JSON line to `anchor_path`, a file outside the database role's reach, so a
//!   database-level rewrite or a rolled-back head is detectable even when the database agrees
//!   with itself. A head that failed verification is never anchored.
//!
//! # A mismatch
//!
//! Sets the service **degraded** (the site stays ready: auditing must not take the site down; the
//! readiness text, the `spot::audit_verify` error event and the alert say so), records the first
//! bad lane, entry index and kind (never entry content) and **stops advancing** the verified head.
//! The kinds are the chain's own codes (`audit.hash_mismatch`, `audit.link_broken`,
//! `audit.seq_gap`, ...) and this module's `audit_verify.anchor_mismatch` and
//! `audit_verify.exactly_once`.
//!
//! # The read model
//!
//! [`AuditVerifyService::verify_now`], [`AuditVerifyService::entries`] and
//! [`AuditVerifyService::export`] are what the audit routes (`security_audit_chain_verify`,
//! `security_audit_entries`, `security_audit_export`) call. Every call asks the [`AuditAccess`]
//! seam first and **fails closed** (an error, a timeout or `false` is a refusal), is bounded, and
//! writes an access record through [`AccessLog`], refusals included. An entry view holds the
//! lane, index, kind, actor, subject DocType and time: never the payload, never a value. The
//! export is a signed bundle (one entry per line, a trailer with the key id and the signature).
//!
//! # Extra chains (WP-SEC-06.T9)
//!
//! Two other tamper-evident chains live on the same store and were never watched: the AI trail
//! (`ai:<site>`, `ai_audit::scope_of`) and the quarantine trail (`quarantine:<site>`,
//! `quarantine::live::scope_of`). `[services.audit_verify] extra_chains` (default both; `[]` turns
//! it off; an unknown name is refused by name) names them, and `audit_verify/extra.rs` verifies
//! each one on the same schedule as the lanes: the incremental pass from its saved verified head,
//! the full pass from entry 1 in slices of `full_slice_entries` with its own saved cursor
//! (additive fields of [`Saved`]: an old state file reads as "not started"). The scope of each is
//! computed by the module that owns the chain, never spelled a second time here. A chain with no
//! entries is clean (a site that never used it). A mismatch degrades the service like a lane's, with the chain's name and
//! the first bad entry index and kind (never content); the readiness text reads `degraded: <kind>
//! in the <name> chain entry <i>`; the chain's verified head stops advancing; and only that
//! chain's own whole clean full walk clears it.
//!
//! **What this does not do.** These chains have no signer, so there is **no anchor and no signed
//! head** for them: they are verified for **internal consistency** only (each entry's hash, its
//! link to the one before it and the sequence). Without an anchor outside the database, a wholesale
//! rewrite of the tail of one of these chains (or a rollback to an earlier head that agrees with
//! itself) is **not detectable by this service**. `verify_now`, the entry views and the export
//! stay on the document lanes: the audit routes are unchanged.
//!
//! # Tenancy
//!
//! One service is one site: it reads only the lanes `<site>#<lane>` of its own site, so another
//! tenant's entries are unreachable and a cursor naming a lane outside the configuration answers
//! like an empty page. The extra chains are the scopes of this site only.
//!
//! # Failure behaviour
//!
//! The service is **optional**. A pass that cannot read the store counts a failure and keeps the
//! verified heads; it never advances them on a read it could not complete. Time comes from the
//! site clock and the keys and signer from the [`AuditKeys`] port; nothing is read from the
//! environment.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use spot_audit_chain::{
    AuditStore, ChainEntry, CheckpointSigner, KeyRing, PayloadValue, Verifier, genesis_hash,
};
use spot_core::Clock;
use spot_doc_audit::{
    AppenderStore, EVENT_ID_KEY, LaneConfig, LaneHead, RECOVERY_WINDOW, SiteCheckpoint, lane_site,
};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "audit_verify/extra.rs"]
pub mod extra;

pub use extra::ExtraChain;

/// Start order: after the capture invariant (1050).
pub const ORDER: u32 = 1070;

/// The most entries one page or one export holds.
pub const MAX_PAGE: usize = 100;

/// The most entries one `verify_now` reads per lane.
pub const MAX_VERIFY_NOW: u64 = 200_000;

/// The anchor does not match the stored heads (a rolled-back or rewritten head).
pub const ANCHOR_MISMATCH: &str = "audit_verify.anchor_mismatch";

/// An event id is chained twice, or the appender's index disagrees with the chain.
pub const EXACTLY_ONCE: &str = "audit_verify.exactly_once";

/// A read-model call was refused (no right, no step-up, a bad cursor or a bound).
pub const REFUSED: &str = "audit_verify.refused";

/// The store, the anchor file or the saved state could not be read or written.
pub const UNAVAILABLE: &str = "audit_verify.unavailable";

/// `[services.audit_verify]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the service runs (off by default: it needs the keys port and an anchor path).
    pub enabled: bool,
    /// The number of lanes of the site's document audit.
    pub lanes: u32,
    /// The lane epoch.
    pub epoch: u32,
    /// Seconds between incremental passes (at least 60).
    pub incremental_seconds: u64,
    /// Seconds between full passes' slices (at least 60).
    pub full_seconds: u64,
    /// Entries one full-pass slice verifies (at least 1).
    pub full_slice_entries: u64,
    /// Seconds between anchors (at least 60).
    pub anchor_seconds: u64,
    /// The file the signed heads are appended to; outside the database role's reach.
    pub anchor_path: Option<String>,
    /// The file the resumable pass state is kept in (defaults to `<anchor_path>.state`).
    pub state_path: Option<String>,
    /// The extra chains verified besides the document lanes: `ai` and `quarantine`. `[]` turns
    /// them off; an unknown name is refused.
    pub extra_chains: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            lanes: 1,
            epoch: 1,
            incremental_seconds: 3_600,
            full_seconds: 600,
            full_slice_entries: 10_000,
            anchor_seconds: 3_600,
            anchor_path: None,
            state_path: None,
            extra_chains: extra::default_names(),
        }
    }
}

impl Settings {
    /// Refuses unusable values, naming the key.
    ///
    /// # Errors
    /// [`Error`] naming the key.
    pub fn validate(&self) -> Result<()> {
        let bad = |key: &str| Error(format!("services.audit_verify.{key} is not usable"));
        if self.lanes == 0 {
            return Err(bad("lanes"));
        }
        if self.full_slice_entries == 0 {
            return Err(bad("full_slice_entries"));
        }
        for (key, value) in [
            ("incremental_seconds", self.incremental_seconds),
            ("full_seconds", self.full_seconds),
            ("anchor_seconds", self.anchor_seconds),
        ] {
            if value < 60 {
                return Err(bad(key));
            }
        }
        extra::parse(&self.extra_chains)?;
        if self.enabled && self.anchor_path.as_deref().is_none_or(str::is_empty) {
            return Err(Error(
                "services.audit_verify.anchor_path is required when the service is enabled"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

/// The signer and the public keys of the audit chain (provided by the module that owns them).
pub trait AuditKeys: Send + Sync {
    /// The signer of anchors and export bundles.
    fn signer(&self) -> Arc<dyn CheckpointSigner>;
    /// The keys that verify what the signer (and its predecessors) signed.
    fn keys(&self) -> KeyRing;
}

/// A second place the application role cannot rewrite: an append-only sink of signed heads.
pub trait AnchorSink: Send + Sync {
    /// Appends one line.
    ///
    /// # Errors
    /// A reason, never a line.
    fn append(&self, line: &str) -> std::result::Result<(), String>;
    /// The newest line, if any.
    ///
    /// # Errors
    /// A reason.
    fn last(&self) -> std::result::Result<Option<String>, String>;
}

/// An anchor sink over a file opened for append.
pub struct FileAnchors(
    /// The file the anchors are appended to.
    pub PathBuf,
);

impl AnchorSink for FileAnchors {
    fn append(&self, line: &str) -> std::result::Result<(), String> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)
            .map_err(|e| format!("the anchor file could not be opened ({})", e.kind()))?;
        writeln!(file, "{line}")
            .map_err(|e| format!("the anchor could not be written ({})", e.kind()))?;
        file.sync_data()
            .map_err(|e| format!("the anchor could not be synced ({})", e.kind()))
    }

    fn last(&self) -> std::result::Result<Option<String>, String> {
        match std::fs::read_to_string(&self.0) {
            Ok(text) => Ok(text
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(str::to_owned)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("the anchor file could not be read ({})", e.kind())),
        }
    }
}

/// What a nightly pass remembers across a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// The last verified head of each lane.
    pub verified: BTreeMap<u32, u64>,
    /// The lane the full pass is on.
    pub cursor_lane: u32,
    /// The next entry of that lane to verify (0 means 1).
    pub cursor_seq: u64,
    /// Whether this full cycle has found anything.
    pub cycle_failed: bool,
    /// The last verified head of each extra chain, by name (`ai`, `quarantine`). Absent in a state
    /// file written before the extra chains: a chain with no entry here is "not started".
    #[serde(default)]
    pub extra_verified: BTreeMap<String, u64>,
    /// The next entry of each extra chain's full walk (absent or 0 means 1).
    #[serde(default)]
    pub extra_cursor: BTreeMap<String, u64>,
    /// Whether each extra chain's current full walk has found anything.
    #[serde(default)]
    pub extra_cycle_failed: BTreeMap<String, bool>,
}

/// Where [`Saved`] is kept.
pub trait StateStore: Send + Sync {
    /// The saved state, if any.
    ///
    /// # Errors
    /// A reason.
    fn load(&self) -> std::result::Result<Option<Saved>, String>;
    /// Saves the state.
    ///
    /// # Errors
    /// A reason.
    fn save(&self, saved: &Saved) -> std::result::Result<(), String>;
}

/// A state store over a JSON file.
pub struct FileState(
    /// The file the state is kept in.
    pub PathBuf,
);

impl StateStore for FileState {
    fn load(&self) -> std::result::Result<Option<Saved>, String> {
        match std::fs::read_to_string(&self.0) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|_| "the saved verification state does not decode".to_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("the saved state could not be read ({})", e.kind())),
        }
    }

    fn save(&self, saved: &Saved) -> std::result::Result<(), String> {
        let text = serde_json::to_string(saved)
            .map_err(|_| "the state could not be encoded".to_owned())?;
        let tmp = self.0.with_extension("tmp");
        std::fs::write(&tmp, text)
            .map_err(|e| format!("the state could not be written ({})", e.kind()))?;
        std::fs::rename(&tmp, &self.0)
            .map_err(|e| format!("the state could not be replaced ({})", e.kind()))
    }
}

/// Who may do what with the audit (the server's permission check; fails closed).
pub trait AuditAccess: Send + Sync {
    /// Whether `caller` holds `security.audit.read` (and, for an export, a recent step-up).
    ///
    /// # Errors
    /// Any error means no: the caller is refused.
    fn may(&self, caller: &str, export: bool) -> std::result::Result<bool, String>;
}

/// One access to the audit, for the audit of the audit: who, what, the outcome. No entry content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    /// The caller.
    pub caller: String,
    /// `verify`, `entries` or `export`.
    pub action: &'static str,
    /// `ok` or the refusal code.
    pub outcome: &'static str,
}

/// Where access records go.
pub trait AccessLog: Send + Sync {
    /// Records one access (refusals too).
    fn record(&self, access: Access);
}

/// An access log that emits one structured `tracing` event, target `spot::audit_verify`.
pub struct TracingLog;

impl AccessLog for TracingLog {
    fn record(&self, access: Access) {
        tracing::info!(target: "spot::audit_verify", caller = %access.caller, action = access.action, outcome = access.outcome, "audit access");
    }
}

/// The first thing wrong: where and what, never content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// The lane, when the finding is about one.
    pub lane: Option<u32>,
    /// The entry index (or the anchored head's index).
    pub seq: u64,
    /// The stable code.
    pub kind: &'static str,
}

/// One entry as a reader sees it: no payload, no value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EntryView {
    /// The lane.
    pub lane: u32,
    /// The index in the lane.
    pub seq: u64,
    /// The record kind (`doc.update`, `secret.set`, ...).
    pub kind: String,
    /// The actor (`user:<id>`, `system`, `agent:<id>`).
    pub actor: String,
    /// The subject's DocType, when the record has one.
    pub subject_doctype: Option<String>,
    /// Unix microseconds.
    pub at: i64,
}

/// One page of entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// The entries, in `(lane, seq)` order.
    pub entries: Vec<EntryView>,
    /// The cursor of the next page, if there is one.
    pub next: Option<String>,
}

/// What a pass did: counts only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Entries whose hash and link were checked.
    pub checked: u64,
    /// Whether the pass found a mismatch.
    pub mismatch: bool,
    /// Whether a full cycle ended this pass.
    pub cycle_done: bool,
}

#[derive(Default)]
struct Inner {
    saved: Saved,
    loaded: bool,
    degraded: Option<Mismatch>,
    /// The extra chain the degraded state comes from (`None` for a document lane or an anchor).
    degraded_chain: Option<ExtraChain>,
    failures: u32,
    last_anchor_at: Option<i64>,
}

/// The audit verification of one site over one chain store.
pub struct AuditVerifyService<S: AuditStore> {
    site: String,
    config: LaneConfig,
    store: Arc<S>,
    keys: Arc<dyn AuditKeys>,
    anchors: Arc<dyn AnchorSink>,
    state: Arc<dyn StateStore>,
    appender: Option<Arc<dyn AppenderStore>>,
    access: Arc<dyn AuditAccess>,
    log: Arc<dyn AccessLog>,
    clock: Arc<dyn Clock>,
    extras: Vec<ExtraChain>,
    settings: Settings,
    inner: Mutex<Inner>,
}

/// Everything the service needs, so a test or a host builds it in one call.
pub struct Wiring<S: AuditStore> {
    /// The site.
    pub site: String,
    /// The chain store.
    pub store: Arc<S>,
    /// Signer and keys.
    pub keys: Arc<dyn AuditKeys>,
    /// The anchor sink.
    pub anchors: Arc<dyn AnchorSink>,
    /// The saved state.
    pub state: Arc<dyn StateStore>,
    /// The appender's durable state, for the exactly-once check (skipped when absent).
    pub appender: Option<Arc<dyn AppenderStore>>,
    /// The permission seam.
    pub access: Arc<dyn AuditAccess>,
    /// The access log.
    pub log: Arc<dyn AccessLog>,
    /// The site clock.
    pub clock: Arc<dyn Clock>,
    /// The settings.
    pub settings: Settings,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl<S: AuditStore + 'static> AuditVerifyService<S> {
    /// The service for `wiring`.
    #[must_use]
    pub fn new(wiring: Wiring<S>) -> Arc<Self> {
        Arc::new(Self {
            config: LaneConfig::new(wiring.settings.lanes, wiring.settings.epoch),
            site: wiring.site,
            store: wiring.store,
            keys: wiring.keys,
            anchors: wiring.anchors,
            state: wiring.state,
            appender: wiring.appender,
            access: wiring.access,
            log: wiring.log,
            clock: wiring.clock,
            extras: extra::chains(&wiring.settings.extra_chains),
            settings: wiring.settings,
            inner: Mutex::new(Inner::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn now_micros(&self) -> i64 {
        i64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
    }

    /// `ok`, or `degraded` with the first mismatch (lane, entry index, kind: never content).
    #[must_use]
    pub fn readiness_text(&self) -> String {
        let inner = self.lock();
        match (inner.degraded, inner.degraded_chain) {
            (None, _) => "ok".to_owned(),
            (Some(m), Some(chain)) => format!(
                "degraded: {} in the {} chain entry {}",
                m.kind,
                chain.name(),
                m.seq
            ),
            (Some(m), None) => format!(
                "degraded: {} at lane {} entry {}",
                m.kind,
                m.lane.map_or_else(|| "-".to_owned(), |l| l.to_string()),
                m.seq
            ),
        }
    }

    /// The first mismatch, if the service is degraded.
    #[must_use]
    pub fn degraded(&self) -> Option<Mismatch> {
        self.lock().degraded
    }

    /// The last verified head of `lane`.
    #[must_use]
    pub fn verified_head(&self, lane: u32) -> u64 {
        self.lock().saved.verified.get(&lane).copied().unwrap_or(0)
    }

    fn load(&self) -> std::result::Result<(), &'static str> {
        if self.lock().loaded {
            return Ok(());
        }
        let saved = self
            .state
            .load()
            .map_err(|_| UNAVAILABLE)?
            .unwrap_or_default();
        let mut inner = self.lock();
        inner.saved = saved;
        inner.loaded = true;
        Ok(())
    }

    fn persist(&self) -> std::result::Result<(), &'static str> {
        let saved = self.lock().saved.clone();
        self.state.save(&saved).map_err(|_| UNAVAILABLE)
    }

    /// Records the first mismatch: degrades the service and raises the high-priority alert (an
    /// error event whose fields are the lane, the entry index and the kind).
    fn flag(&self, mismatch: Mismatch) {
        let first = {
            let mut inner = self.lock();
            inner.saved.cycle_failed = true;
            let first = inner.degraded.is_none();
            if first {
                inner.degraded = Some(mismatch);
            }
            first
        };
        if first {
            tracing::error!(target: "spot::audit_verify", site = %self.site, kind = mismatch.kind, lane = ?mismatch.lane, seq = mismatch.seq, "AUDIT CHAIN MISMATCH");
        }
    }

    /// Verifies `from..=to` of one lane. Returns the entries checked, or flags the first failure.
    async fn verify_lane(
        &self,
        lane: u32,
        from: u64,
        to: u64,
    ) -> std::result::Result<(u64, bool), &'static str> {
        let keys = self.keys.keys();
        let site = lane_site(&self.site, lane);
        let report = Verifier::new(&keys)
            .verify_range(&*self.store, &site, from, to)
            .await
            .map_err(|_| UNAVAILABLE)?;
        if let Some(first) = report.failures.first() {
            self.flag(Mismatch {
                lane: Some(lane),
                seq: first.seq,
                kind: first.kind.code(),
            });
            return Ok((report.checked, false));
        }
        Ok((report.checked, true))
    }

    /// One incremental pass: every lane from its verified head, then the anchor and the
    /// exactly-once check.
    ///
    /// # Errors
    /// [`UNAVAILABLE`] when the store, the anchor file or the saved state cannot be read.
    pub async fn run_incremental(&self) -> std::result::Result<PassReport, &'static str> {
        self.load()?;
        let mut report = PassReport::default();
        for lane in self.config.lanes() {
            let from = self.verified_head(lane).saturating_add(1);
            let (checked, clean) = self.verify_lane(lane, from, u64::MAX).await?;
            report.checked = report.checked.saturating_add(checked);
            if clean && self.degraded().is_none() {
                let head = self
                    .store
                    .head(&lane_site(&self.site, lane))
                    .await
                    .map_err(|_| UNAVAILABLE)?;
                // Never backwards: a head lower than the verified one is for the anchor to judge.
                if let Some(head) = head.filter(|h| h.seq > self.verified_head(lane)) {
                    self.lock().saved.verified.insert(lane, head.seq);
                }
            }
        }
        // The extra chains (AI, quarantine): internal consistency only, no anchor.
        let extra_checked = self.run_extra_incremental().await?;
        report.checked = report.checked.saturating_add(extra_checked);
        self.check_anchor().await?;
        self.check_exactly_once().await?;
        report.mismatch = self.degraded().is_some();
        self.persist()?;
        Ok(report)
    }

    /// One slice of the nightly full pass, resumable. The cursor is saved after the slice.
    ///
    /// # Errors
    /// [`UNAVAILABLE`] as [`AuditVerifyService::run_incremental`].
    pub async fn run_full_slice(&self) -> std::result::Result<PassReport, &'static str> {
        self.load()?;
        let (lane, from) = {
            let inner = self.lock();
            (
                inner.saved.cursor_lane.min(self.config.n.saturating_sub(1)),
                inner.saved.cursor_seq.max(1),
            )
        };
        let to = from.saturating_add(self.settings.full_slice_entries - 1);
        let (checked, _) = self.verify_lane(lane, from, to).await?;
        let head = self
            .store
            .head(&lane_site(&self.site, lane))
            .await
            .map_err(|_| UNAVAILABLE)?;
        let lane_done = head.is_none_or(|h| h.seq <= to);
        let mut report = PassReport {
            checked,
            mismatch: self.degraded().is_some(),
            cycle_done: false,
        };
        {
            let mut inner = self.lock();
            if lane_done && lane + 1 >= self.config.n {
                report.cycle_done = true;
                inner.saved.cursor_lane = 0;
                inner.saved.cursor_seq = 1;
                // Only a whole clean cycle clears a degraded state, and it clears the cycle flag.
                // A state that comes from an extra chain is that chain's own to clear.
                if !inner.saved.cycle_failed && inner.degraded_chain.is_none() {
                    inner.degraded = None;
                }
                inner.saved.cycle_failed = false;
            } else if lane_done {
                inner.saved.cursor_lane = lane + 1;
                inner.saved.cursor_seq = 1;
            } else {
                inner.saved.cursor_seq = to.saturating_add(1);
            }
        }
        // One slice of each extra chain's own walk, with its own saved cursor.
        let (extra_checked, extra_found) = self.run_extra_full_slice().await?;
        report.checked = report.checked.saturating_add(extra_checked);
        report.mismatch |= extra_found;
        self.persist()?;
        Ok(report)
    }

    /// Writes the signed head of every lane to the anchor sink, unless the chain is degraded.
    ///
    /// # Errors
    /// [`UNAVAILABLE`] when a head cannot be read or the anchor cannot be written.
    pub async fn write_anchor(&self) -> std::result::Result<bool, &'static str> {
        if self.degraded().is_some() {
            return Ok(false);
        }
        let mut heads = Vec::new();
        for lane in self.config.lanes() {
            let site = lane_site(&self.site, lane);
            let (seq, head_hash) = match self.store.head(&site).await.map_err(|_| UNAVAILABLE)? {
                Some(h) => (h.seq, h.hash),
                None => (0, genesis_hash(&site)),
            };
            heads.push(LaneHead {
                lane,
                seq,
                head_hash,
            });
        }
        let at = self.now_micros();
        let signed =
            SiteCheckpoint::sign(&*self.keys.signer(), &self.site, &self.config, &heads, at)
                .map_err(|_| UNAVAILABLE)?;
        let line = serde_json::to_string(&signed).map_err(|_| UNAVAILABLE)?;
        self.anchors.append(&line).map_err(|_| UNAVAILABLE)?;
        self.lock().last_anchor_at = Some(at);
        Ok(true)
    }

    /// Compares the newest anchor with the stored chain: its signature must verify and every
    /// anchored head must still be in the store with the same hash (a rolled-back head, a
    /// truncated tail or a rewritten entry all fail here).
    async fn check_anchor(&self) -> std::result::Result<(), &'static str> {
        let Some(line) = self.anchors.last().map_err(|_| UNAVAILABLE)? else {
            return Ok(());
        };
        let anchor: SiteCheckpoint = match serde_json::from_str(&line) {
            Ok(a) => a,
            Err(_) => {
                self.flag(Mismatch {
                    lane: None,
                    seq: 0,
                    kind: ANCHOR_MISMATCH,
                });
                return Ok(());
            }
        };
        if anchor
            .verify(&self.keys.keys(), &self.site, &self.config)
            .is_err()
        {
            self.flag(Mismatch {
                lane: None,
                seq: anchor.signed.seq,
                kind: ANCHOR_MISMATCH,
            });
            return Ok(());
        }
        for head in &anchor.lanes {
            if head.seq == 0 {
                continue;
            }
            let entries = self
                .store
                .range(&lane_site(&self.site, head.lane), head.seq, head.seq)
                .await
                .map_err(|_| UNAVAILABLE)?;
            if !entries
                .iter()
                .any(|e| e.seq == head.seq && e.hash == head.head_hash)
            {
                self.flag(Mismatch {
                    lane: Some(head.lane),
                    seq: head.seq,
                    kind: ANCHOR_MISMATCH,
                });
            }
        }
        Ok(())
    }

    /// Looks at the newest entries of each lane: an event id chained twice, or an entry the
    /// appender's index places elsewhere, is a mismatch. The newest entry may be unindexed (its
    /// index write may still be in flight), so it is not required to be indexed.
    async fn check_exactly_once(&self) -> std::result::Result<(), &'static str> {
        for lane in self.config.lanes() {
            let site = lane_site(&self.site, lane);
            let Some(head) = self.store.head(&site).await.map_err(|_| UNAVAILABLE)? else {
                continue;
            };
            let from = head.seq.saturating_sub(RECOVERY_WINDOW * 2 - 1).max(1);
            let entries = self
                .store
                .range(&site, from, head.seq)
                .await
                .map_err(|_| UNAVAILABLE)?;
            let mut seen: HashSet<String> = HashSet::new();
            for entry in &entries {
                let Some(PayloadValue::Text(id)) = entry.record.payload.get(EVENT_ID_KEY) else {
                    continue;
                };
                if !seen.insert(id.clone()) {
                    self.flag(Mismatch {
                        lane: Some(lane),
                        seq: entry.seq,
                        kind: EXACTLY_ONCE,
                    });
                    continue;
                }
                if let Some(appender) = &self.appender
                    && entry.seq < head.seq
                    && appender.appended(id).await.map_err(|_| UNAVAILABLE)?
                        != Some((lane, entry.seq))
                {
                    self.flag(Mismatch {
                        lane: Some(lane),
                        seq: entry.seq,
                        kind: EXACTLY_ONCE,
                    });
                }
            }
        }
        Ok(())
    }

    fn authorise(
        &self,
        caller: &str,
        action: &'static str,
        export: bool,
    ) -> std::result::Result<(), &'static str> {
        let allowed = matches!(self.access.may(caller, export), Ok(true));
        if !allowed {
            self.log.record(Access {
                caller: caller.to_owned(),
                action,
                outcome: REFUSED,
            });
            return Err(REFUSED);
        }
        Ok(())
    }

    /// Verifies every lane now (bounded), for `caller`. Does not change the degraded state.
    ///
    /// # Errors
    /// [`REFUSED`] when the caller may not, [`UNAVAILABLE`] when the store cannot be read.
    pub async fn verify_now(
        &self,
        caller: &str,
    ) -> std::result::Result<(Vec<(u32, u64)>, Option<Mismatch>), &'static str> {
        self.authorise(caller, "verify", false)?;
        let keys = self.keys.keys();
        let mut checked = Vec::new();
        let mut first = None;
        for lane in self.config.lanes() {
            let report = Verifier::new(&keys)
                .verify_range(
                    &*self.store,
                    &lane_site(&self.site, lane),
                    1,
                    MAX_VERIFY_NOW,
                )
                .await
                .map_err(|_| {
                    self.log.record(Access {
                        caller: caller.to_owned(),
                        action: "verify",
                        outcome: UNAVAILABLE,
                    });
                    UNAVAILABLE
                })?;
            checked.push((lane, report.checked));
            if first.is_none() {
                first = report.failures.first().map(|f| Mismatch {
                    lane: Some(lane),
                    seq: f.seq,
                    kind: f.kind.code(),
                });
            }
        }
        self.log.record(Access {
            caller: caller.to_owned(),
            action: "verify",
            outcome: "ok",
        });
        Ok((checked, first))
    }

    fn view(lane: u32, entry: &ChainEntry) -> EntryView {
        EntryView {
            lane,
            seq: entry.seq,
            kind: entry.record.kind.clone(),
            actor: entry.record.actor.clone(),
            subject_doctype: entry.record.subject.as_ref().map(|s| s.doctype.clone()),
            at: entry.record.at,
        }
    }

    /// A page of masked entries with `from <= at <= to` (microseconds), in `(lane, seq)` order.
    /// `cursor` is the previous page's `next` (`<lane>.<seq>`); a cursor outside the configured
    /// lanes answers an empty page, like a lane with nothing in it.
    ///
    /// # Errors
    /// [`REFUSED`] for a caller without the right or a malformed cursor or limit,
    /// [`UNAVAILABLE`] when the store cannot be read.
    pub async fn entries(
        &self,
        caller: &str,
        from: i64,
        to: i64,
        cursor: Option<&str>,
        limit: usize,
    ) -> std::result::Result<Page, &'static str> {
        self.authorise(caller, "entries", false)?;
        let done = self.page(from, to, cursor, limit).await;
        let outcome = done.as_ref().map_or_else(|code| *code, |_| "ok");
        self.log.record(Access {
            caller: caller.to_owned(),
            action: "entries",
            outcome,
        });
        done
    }

    /// The signed export of `from..=to`: one masked entry per line, then a trailer line with the
    /// key id and the signature over every byte before it. Needs the export right (which includes
    /// the recent step-up the route checks). Bounded to `MAX_PAGE * 100` entries.
    ///
    /// # Errors
    /// [`REFUSED`] or [`UNAVAILABLE`] as [`AuditVerifyService::entries`].
    pub async fn export(
        &self,
        caller: &str,
        from: i64,
        to: i64,
    ) -> std::result::Result<String, &'static str> {
        self.authorise(caller, "export", true)?;
        let done = self.signed_bundle(from, to).await;
        let outcome = done.as_ref().map_or_else(|code| *code, |_| "ok");
        self.log.record(Access {
            caller: caller.to_owned(),
            action: "export",
            outcome,
        });
        done
    }

    async fn signed_bundle(&self, from: i64, to: i64) -> std::result::Result<String, &'static str> {
        let mut body = String::new();
        let mut cursor: Option<String> = None;
        let mut count = 0usize;
        loop {
            let page = self.page(from, to, cursor.as_deref(), MAX_PAGE).await?;
            for view in &page.entries {
                body.push_str(&serde_json::to_string(view).map_err(|_| UNAVAILABLE)?);
                body.push('\n');
                count += 1;
            }
            cursor = page.next;
            if cursor.is_none() || count >= MAX_PAGE * 100 {
                break;
            }
        }
        let signer = self.keys.signer();
        let signature = signer.sign(body.as_bytes()).map_err(|_| UNAVAILABLE)?;
        body.push_str(&format!(
            "{{\"key_id\":\"{}\",\"signature\":\"{}\"}}\n",
            signer.key_id(),
            hex(&signature)
        ));
        Ok(body)
    }

    /// One page, after the permission check: bounded, from a cursor the caller cannot forge into
    /// another site (a lane outside the configuration is an empty page).
    async fn page(
        &self,
        from: i64,
        to: i64,
        cursor: Option<&str>,
        limit: usize,
    ) -> std::result::Result<Page, &'static str> {
        if limit == 0 || limit > MAX_PAGE || from > to {
            return Err(REFUSED);
        }
        let (mut lane, mut seq) = match cursor {
            None => (0, 1),
            Some(text) => text
                .split_once('.')
                .and_then(|(l, s)| Some((l.parse::<u32>().ok()?, s.parse::<u64>().ok()?)))
                .ok_or(REFUSED)?,
        };
        let mut out = Vec::new();
        while out.len() < limit && lane < self.config.n {
            let first = seq.max(1);
            let entries = self
                .store
                .range(
                    &lane_site(&self.site, lane),
                    first,
                    first.saturating_add(399),
                )
                .await
                .map_err(|_| UNAVAILABLE)?;
            if entries.is_empty() {
                lane += 1;
                seq = 1;
                continue;
            }
            for entry in &entries {
                if out.len() == limit {
                    break;
                }
                seq = entry.seq.saturating_add(1);
                if entry.record.at >= from && entry.record.at <= to {
                    out.push(Self::view(lane, entry));
                }
            }
        }
        let next = (lane < self.config.n).then(|| format!("{lane}.{seq}"));
        Ok(Page { entries: out, next })
    }
}

impl<S: AuditStore + 'static> Service for AuditVerifyService<S> {
    fn name(&self) -> &'static str {
        "audit_verify"
    }

    fn level(&self) -> Level {
        // Auditing must not take the site down: a mismatch degrades and alerts.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.load()
                .map_err(|_| "the saved audit verification state could not be read".to_owned())?;
            self.anchors
                .last()
                .map(|_| ())
                .map_err(|e| format!("the audit anchor sink is not usable: {e}"))
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut incremental =
                tokio::time::interval(Duration::from_secs(self.settings.incremental_seconds));
            let mut full = tokio::time::interval(Duration::from_secs(self.settings.full_seconds));
            let mut anchor =
                tokio::time::interval(Duration::from_secs(self.settings.anchor_seconds));
            for ticker in [&mut incremental, &mut full, &mut anchor] {
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            }
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    _ = incremental.tick() => self.count(self.run_incremental().await.map(|_| ())),
                    _ = full.tick() => self.count(self.run_full_slice().await.map(|_| ())),
                    _ = anchor.tick() => self.count(self.write_anchor().await.map(|_| ())),
                }
            }
        })
    }
}

impl<S: AuditStore + 'static> AuditVerifyService<S> {
    /// Counts a failed pass (a store that cannot be read); the loop goes on.
    fn count(&self, result: std::result::Result<(), &'static str>) {
        let mut inner = self.lock();
        match result {
            Ok(()) => inner.failures = 0,
            Err(code) => {
                inner.failures = inner.failures.saturating_add(1);
                tracing::warn!(target: "spot::audit_verify", site = %self.site, code, failures = inner.failures, "an audit verification pass could not complete");
            }
        }
    }
}

/// Registers the service when `[services.audit_verify] enabled = true`: the keys port is required
/// (a finaliser builds the service once the module that owns the keys has provided it), and the
/// anchor sink and the saved state are files named by the settings.
///
/// # Errors
/// [`Error`] when the settings are unusable or the keys port is missing.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("audit_verify")?;
    settings.validate()?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let backend = env.backend.clone();
    let clock: Arc<dyn Clock> = Arc::clone(ctx.clock());
    ctx.finally(move |ctx| {
        let keys = ctx.require_port::<dyn AuditKeys>()?;
        let access = ctx.require_port::<dyn AuditAccess>()?;
        let anchor_path = PathBuf::from(settings.anchor_path.clone().unwrap_or_default());
        let state_path = settings
            .state_path
            .clone()
            .map_or_else(|| anchor_path.with_extension("state"), PathBuf::from);
        let service = AuditVerifyService::new(Wiring {
            site,
            store: Arc::new(spot_audit_store::PgAuditStore::new(backend.pool())),
            keys,
            anchors: Arc::new(FileAnchors(anchor_path)),
            state: Arc::new(FileState(state_path)),
            appender: ctx.port::<dyn AppenderStore>(),
            access,
            log: Arc::new(TracingLog),
            clock,
            settings,
        });
        ctx.add(Arc::clone(&service) as Arc<dyn Service>);
        ctx.provide::<AuditVerifyService<spot_audit_store::PgAuditStore>>(service)?;
        Ok(())
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_audit_chain::{
        AppendOutcome, AuditChain, AuditRecord, Checkpoint, CheckpointPolicy, InMemoryStore,
        LocalSigner, StoreFuture,
    };
    use spot_core::ManualClock;
    use spot_doc_audit::InMemoryAppenderStore;

    const SITE: &str = "site-a";

    /// A chain store that can show a damaged view of the chain, as a rewritten database would.
    struct Damaged {
        inner: Arc<InMemoryStore>,
        #[allow(clippy::type_complexity)]
        edit: Mutex<Option<Box<dyn Fn(Vec<ChainEntry>) -> Vec<ChainEntry> + Send + Sync>>>,
    }

    impl Damaged {
        /// The damage is done to the document lanes only: the extra chains (the AI and the
        /// quarantine trail, WP-SEC-06.T9) are read through the same store and are empty here,
        /// and an edit written for the lane's five entries must not see them.
        fn apply(&self, scope: &str, entries: Vec<ChainEntry>) -> Vec<ChainEntry> {
            if super::extra::ExtraChain::ALL
                .iter()
                .any(|chain| chain.scope(SITE) == scope)
            {
                return entries;
            }
            match &*self.edit.lock().unwrap() {
                Some(edit) => edit(entries),
                None => entries,
            }
        }
    }

    impl AuditStore for Damaged {
        fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
            Box::pin(async move {
                let all = self.inner.range(site, 1, u64::MAX).await?;
                Ok(self.apply(site, all).pop())
            })
        }
        fn append_if_head<'a>(
            &'a self,
            site: &'a str,
            expected_seq: u64,
            entry: ChainEntry,
        ) -> StoreFuture<'a, AppendOutcome> {
            self.inner.append_if_head(site, expected_seq, entry)
        }
        fn range<'a>(
            &'a self,
            site: &'a str,
            from: u64,
            to: u64,
        ) -> StoreFuture<'a, Vec<ChainEntry>> {
            Box::pin(async move {
                let all = self.inner.range(site, 1, u64::MAX).await?;
                Ok(self
                    .apply(site, all)
                    .into_iter()
                    .filter(|e| e.seq >= from && e.seq <= to)
                    .collect())
            })
        }
        fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
            self.inner.put_checkpoint(checkpoint)
        }
        fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
            self.inner.latest_checkpoint(site)
        }
    }

    struct Keys(Arc<LocalSigner>);
    impl AuditKeys for Keys {
        fn signer(&self) -> Arc<dyn CheckpointSigner> {
            self.0.clone()
        }
        fn keys(&self) -> KeyRing {
            let mut ring = KeyRing::new();
            ring.add(&self.0.public_key()).unwrap();
            ring
        }
    }

    #[derive(Default)]
    struct MemAnchors(Mutex<Vec<String>>);
    impl AnchorSink for MemAnchors {
        fn append(&self, line: &str) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(line.to_owned());
            Ok(())
        }
        fn last(&self) -> std::result::Result<Option<String>, String> {
            Ok(self.0.lock().unwrap().last().cloned())
        }
    }

    #[derive(Default)]
    struct MemState(Mutex<Option<Saved>>);
    impl StateStore for MemState {
        fn load(&self) -> std::result::Result<Option<Saved>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn save(&self, saved: &Saved) -> std::result::Result<(), String> {
            *self.0.lock().unwrap() = Some(saved.clone());
            Ok(())
        }
    }

    struct Auditors(&'static [&'static str]);
    impl AuditAccess for Auditors {
        fn may(&self, caller: &str, _export: bool) -> std::result::Result<bool, String> {
            Ok(self.0.contains(&caller))
        }
    }

    #[derive(Default)]
    struct Records(Mutex<Vec<Access>>);
    impl AccessLog for Records {
        fn record(&self, access: Access) {
            self.0.lock().unwrap().push(access);
        }
    }

    struct Rig {
        raw: Arc<InMemoryStore>,
        store: Arc<Damaged>,
        anchors: Arc<MemAnchors>,
        state: Arc<MemState>,
        records: Arc<Records>,
        signer: Arc<LocalSigner>,
        appender: Arc<InMemoryAppenderStore>,
        service: Arc<AuditVerifyService<Damaged>>,
    }

    fn record(n: u64, event_id: Option<&str>) -> AuditRecord {
        let mut payload = BTreeMap::new();
        payload.insert("n".to_owned(), PayloadValue::Int(i64::try_from(n).unwrap()));
        if let Some(id) = event_id {
            payload.insert(EVENT_ID_KEY.to_owned(), PayloadValue::Text(id.to_owned()));
        }
        AuditRecord {
            kind: "doc.update".to_owned(),
            actor: "user:u1".to_owned(),
            on_behalf_of: None,
            subject: Some(spot_audit_chain::Subject {
                doctype: "Invoice".to_owned(),
                id: format!("INV-{n}"),
            }),
            at: 1_000 + i64::try_from(n).unwrap(),
            payload,
            masked: true,
        }
    }

    async fn rig(entries: u64, slice: u64) -> Rig {
        let raw = Arc::new(InMemoryStore::new());
        let signer = Arc::new(LocalSigner::from_seed([4u8; 32]));
        let chain = AuditChain::new(
            spot_doc_audit::SharedStore::new(Arc::clone(&raw)),
            lane_site(SITE, 0),
            signer.clone(),
            CheckpointPolicy::default(),
        );
        for n in 1..=entries {
            chain
                .append(record(n, Some(&format!("ev-{n}"))))
                .await
                .unwrap();
        }
        let store = Arc::new(Damaged {
            inner: Arc::clone(&raw),
            edit: Mutex::new(None),
        });
        let anchors = Arc::new(MemAnchors::default());
        let state = Arc::new(MemState::default());
        let records = Arc::new(Records::default());
        let appender = Arc::new(InMemoryAppenderStore::new());
        for n in 1..=entries {
            appender
                .record_appended(&format!("ev-{n}"), 0, n)
                .await
                .unwrap();
        }
        let service = Rig::service(
            &store, &anchors, &state, &records, &signer, &appender, slice,
        );
        Rig {
            raw,
            store,
            anchors,
            state,
            records,
            signer,
            appender,
            service,
        }
    }

    impl Rig {
        fn service(
            store: &Arc<Damaged>,
            anchors: &Arc<MemAnchors>,
            state: &Arc<MemState>,
            records: &Arc<Records>,
            signer: &Arc<LocalSigner>,
            appender: &Arc<InMemoryAppenderStore>,
            slice: u64,
        ) -> Arc<AuditVerifyService<Damaged>> {
            AuditVerifyService::new(Wiring {
                site: SITE.to_owned(),
                store: Arc::clone(store),
                keys: Arc::new(Keys(Arc::clone(signer))),
                anchors: anchors.clone(),
                state: state.clone(),
                appender: Some(appender.clone() as Arc<dyn AppenderStore>),
                access: Arc::new(Auditors(&["auditor"])),
                log: records.clone(),
                clock: Arc::new(ManualClock::new(
                    spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
                )),
                settings: Settings {
                    full_slice_entries: slice,
                    ..Settings::default()
                },
            })
        }

        fn damage(
            &self,
            edit: impl Fn(Vec<ChainEntry>) -> Vec<ChainEntry> + Send + Sync + 'static,
        ) {
            *self.store.edit.lock().unwrap() = Some(Box::new(edit));
        }
    }

    #[tokio::test]
    async fn a_clean_chain_verifies_and_the_verified_head_advances() {
        let r = rig(5, 100).await;
        let report = r.service.run_incremental().await.unwrap();
        assert_eq!(
            report,
            PassReport {
                checked: 5,
                mismatch: false,
                cycle_done: false
            }
        );
        assert_eq!(r.service.verified_head(0), 5);
        assert_eq!(r.service.readiness_text(), "ok");
        assert!(r.service.write_anchor().await.unwrap());
        assert!(!r.service.run_incremental().await.unwrap().mismatch);
    }

    #[tokio::test]
    async fn an_altered_entry_is_flagged_at_exactly_its_index_and_the_head_stops() {
        let r = rig(5, 100).await;
        r.service.run_incremental().await.unwrap();
        r.damage(|mut entries| {
            entries[2].record.actor = "user:mallory".to_owned();
            entries
        });
        r.service.run_incremental().await.unwrap();
        // The pass from the verified head (5) sees nothing new; the full pass finds it.
        r.service.run_full_slice().await.unwrap();
        assert_eq!(
            r.service.degraded(),
            Some(Mismatch {
                lane: Some(0),
                seq: 3,
                kind: "audit.hash_mismatch"
            })
        );
        assert!(
            r.service
                .readiness_text()
                .starts_with("degraded: audit.hash_mismatch at lane 0 entry 3")
        );
        assert!(
            !r.service.write_anchor().await.unwrap(),
            "a damaged head is never anchored"
        );
    }

    #[tokio::test]
    async fn a_deleted_entry_and_a_reorder_are_found() {
        let r = rig(5, 100).await;
        r.damage(|mut entries| {
            entries.remove(2);
            entries
        });
        r.service.run_incremental().await.unwrap();
        assert_eq!(r.service.degraded().map(|m| m.kind), Some("audit.seq_gap"));

        let r = rig(5, 100).await;
        r.damage(|mut entries| {
            entries.swap(1, 2);
            entries
        });
        r.service.run_incremental().await.unwrap();
        assert!(r.service.degraded().is_some(), "a reorder breaks the links");
    }

    #[tokio::test]
    async fn a_rolled_back_head_is_caught_by_the_anchor() {
        let r = rig(5, 100).await;
        r.service.run_incremental().await.unwrap();
        assert!(r.service.write_anchor().await.unwrap());
        // The database loses its last two entries and agrees with itself.
        r.damage(|mut entries| {
            entries.truncate(3);
            entries
        });
        r.service.run_incremental().await.unwrap();
        assert_eq!(
            r.service.degraded(),
            Some(Mismatch {
                lane: Some(0),
                seq: 5,
                kind: ANCHOR_MISMATCH
            })
        );
    }

    #[tokio::test]
    async fn a_forged_anchor_is_a_mismatch() {
        let r = rig(2, 100).await;
        r.anchors.append("{\"not\":\"an anchor\"}").unwrap();
        r.service.run_incremental().await.unwrap();
        assert_eq!(r.service.degraded().map(|m| m.kind), Some(ANCHOR_MISMATCH));
    }

    #[tokio::test]
    async fn an_event_chained_twice_or_indexed_elsewhere_is_flagged() {
        let r = rig(0, 100).await;
        let chain = AuditChain::new(
            spot_doc_audit::SharedStore::new(Arc::clone(&r.raw)),
            lane_site(SITE, 0),
            r.signer.clone(),
            CheckpointPolicy::default(),
        );
        chain.append(record(1, Some("ev-1"))).await.unwrap();
        chain.append(record(2, Some("ev-1"))).await.unwrap();
        r.appender.record_appended("ev-1", 0, 1).await.unwrap();
        r.service.run_incremental().await.unwrap();
        assert_eq!(
            r.service.degraded(),
            Some(Mismatch {
                lane: Some(0),
                seq: 2,
                kind: EXACTLY_ONCE
            })
        );

        let r = rig(3, 100).await;
        r.appender.record_appended("ev-9", 0, 9).await.unwrap();
        // The index places ev-1 at another entry than the chain does.
        let other = InMemoryAppenderStore::new();
        other.record_appended("ev-1", 0, 2).await.unwrap();
        let swapped = AuditVerifyService::new(Wiring {
            site: SITE.to_owned(),
            store: Arc::clone(&r.store),
            keys: Arc::new(Keys(Arc::clone(&r.signer))),
            anchors: r.anchors.clone(),
            state: Arc::new(MemState::default()),
            appender: Some(Arc::new(other) as Arc<dyn AppenderStore>),
            access: Arc::new(Auditors(&[])),
            log: r.records.clone(),
            clock: Arc::new(ManualClock::new(
                spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
            )),
            settings: Settings::default(),
        });
        swapped.run_incremental().await.unwrap();
        assert_eq!(swapped.degraded().map(|m| m.kind), Some(EXACTLY_ONCE));
    }

    #[tokio::test]
    async fn the_nightly_pass_resumes_after_a_restart_and_a_clean_cycle_clears_the_alert() {
        let r = rig(5, 2).await;
        r.damage(|mut entries| {
            entries[3].record.actor = "user:mallory".to_owned();
            entries
        });
        // Slice 1 (entries 1-2) is clean; the service restarts; slice 2 (3-4) finds entry 4.
        r.service.run_full_slice().await.unwrap();
        let restarted = Rig::service(
            &r.store,
            &r.anchors,
            &r.state,
            &r.records,
            &r.signer,
            &r.appender,
            2,
        );
        let slice = restarted.run_full_slice().await.unwrap();
        assert!(slice.mismatch, "the resumed slice found the entry");
        assert_eq!(restarted.degraded().map(|m| m.seq), Some(4));
        // The rest of the cycle completes; a failed cycle does not clear the alert.
        restarted.run_full_slice().await.unwrap();
        assert!(restarted.degraded().is_some());
        // The damage is repaired; the next whole clean cycle clears it.
        *r.store.edit.lock().unwrap() = None;
        for _ in 0..3 {
            restarted.run_full_slice().await.unwrap();
        }
        assert_eq!(restarted.degraded(), None);
    }

    #[tokio::test]
    async fn the_read_model_refuses_a_non_auditor_masks_and_is_bounded() {
        let r = rig(3, 100).await;
        let denied = r.service.entries("mallory", 0, i64::MAX, None, 10).await;
        assert_eq!(denied, Err(REFUSED));
        assert_eq!(r.service.export("mallory", 0, i64::MAX).await, Err(REFUSED));
        assert_eq!(
            r.service.verify_now("mallory").await.map(|_| ()),
            Err(REFUSED)
        );
        assert!(
            r.records
                .0
                .lock()
                .unwrap()
                .iter()
                .all(|a| a.outcome == REFUSED),
            "refusals are recorded too"
        );

        let page = r
            .service
            .entries("auditor", 0, i64::MAX, None, 2)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.next.as_deref(), Some("0.3"));
        let text = format!("{page:?}");
        assert!(
            !text.contains("payload") && !text.contains("ev-1") && !text.contains("INV-1"),
            "{text}"
        );
        let rest = r
            .service
            .entries("auditor", 0, i64::MAX, page.next.as_deref(), 2)
            .await
            .unwrap();
        assert_eq!(rest.entries.len(), 1);
        assert_eq!(
            r.service
                .entries("auditor", 0, i64::MAX, None, MAX_PAGE + 1)
                .await,
            Err(REFUSED)
        );
        assert_eq!(
            r.service
                .entries("auditor", 0, i64::MAX, Some("nonsense"), 5)
                .await,
            Err(REFUSED)
        );
        // A cursor into a lane outside the configuration is an empty page: another tenant's lanes are unreachable.
        let beyond = r
            .service
            .entries("auditor", 0, i64::MAX, Some("7.1"), 5)
            .await
            .unwrap();
        assert!(beyond.entries.is_empty() && beyond.next.is_none());

        let bundle = r.service.export("auditor", 0, i64::MAX).await.unwrap();
        let (body, trailer) = bundle.trim_end().rsplit_once('\n').unwrap();
        assert_eq!(body.lines().count(), 3);
        assert!(trailer.contains("\"signature\""));
    }

    #[test]
    fn the_settings_refuse_nonsense() {
        assert!(Settings::default().validate().is_ok());
        assert!(
            Settings {
                enabled: true,
                ..Settings::default()
            }
            .validate()
            .is_err(),
            "an anchor path is required"
        );
        assert!(
            Settings {
                lanes: 0,
                ..Settings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Settings {
                full_seconds: 5,
                ..Settings::default()
            }
            .validate()
            .is_err()
        );
    }
}
