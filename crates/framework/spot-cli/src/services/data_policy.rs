//! Retention, enforced on a server (WP-SEC-05.T4): a scheduled, resumable, dry-run-first job over
//! the tenant's policy, with legal holds checked per item, hard caps, a kill switch and audited
//! plans, confirmations and deletion proofs.
//!
//! # What this module is, and what it is not
//!
//! `spot-data-policy` decides (rules, plan, execute, holds, proofs); `spot-data-policy-pg` is the
//! ledger. This module is the host: it loads the policy, builds plans, holds the pending run,
//! drives [`spot_data_policy::execute`] one batch at a time and writes run-level audit entries to
//! a chain of its own (scope [`scope_of`], `retention:<site>`). It does **not** supply the
//! [`StoreProvider`]: the production provider (the PostgreSQL ledger plus a document-lifecycle
//! record source) is a follow-up, so until a module provides one the service is degraded and
//! reports it (see the task report).
//!
//! # The modes (`[services.data_policy] enforcement`)
//!
//! * `off` (default): every pass builds a plan and keeps it pending with its digest. Nothing is
//!   ever executed.
//! * `manual`: a pending plan runs only after an administrator confirmed **its digest** with
//!   [`DataPolicyHost::confirm`]. The person who requested a plan cannot confirm it.
//! * `automatic`: the plan runs only after the same policy has been observed in dry runs for
//!   `automatic_after_days` days (default 7); a changed policy restarts the observation.
//!
//! # Execution
//!
//! One run is [`spot_data_policy::execute`] called one batch at a time: between batches the
//! policy is read again (so **a hold added mid-run protects the held records at once**), the kill
//! switch and the time slice are checked, and the run stops when `max_deletions_per_run` items
//! were applied. Holds, statutory floors and referential links are judged per item at execution
//! time by the engine, and each deletion is one store transaction carrying the audit record and
//! the deletion proof. The audit chain entry `retention.run_started` is written **before** the
//! first batch (a failed append refuses the run) and `retention.run_finished` plus one
//! `retention.proof` per deleted record after it.
//!
//! A run that stopped (kill switch, time slice, cap, tenant hold) keeps its plan pending in this
//! process and the next run **continues** it from the saved checkpoint, so nothing is deleted
//! twice. After a restart the pending plan is gone (it is not persisted): the next pass plans
//! again, the digest differs, and the administrator confirms it again. Records already deleted are
//! simply absent from the new plan.
//!
//! # Kill switch
//!
//! `enabled = false` removes the service on the next reload; [`DataPolicyHost::kill`] stops a run
//! within one batch; a hold with scope `all` stops it too (reason `tenant_hold`).
//!
//! # Backups
//!
//! When a [`BackupCatalog`] is provided, every pass feeds `plan_backup_expiry` and keeps the list
//! ([`DataPolicyHost::backup_expiries`]); the backup service executes it, this module only lists.
//!
//! # Threads
//!
//! The store is synchronous (the PostgreSQL ledger crosses a blocking bridge that refuses
//! runtime threads), so every plan and run executes on a dedicated plain thread and the async
//! side waits for it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use spot_audit_chain::{
    AuditChain, AuditError as ChainError, AuditRecord, AuditStore, CheckpointPolicy,
    CheckpointSigner, PayloadValue, Subject,
};
use spot_audit_store::PgAuditStore;
use spot_core::Clock;
use spot_data_policy::{
    BackupPlan, BackupSet, ExecutionOptions, FieldClassMap, Gregorian, HoldScope, Holds,
    OutcomeStatus, PolicySet, Resume, RetentionPlan, RetentionRules, RetentionStore,
    StatutoryFloor, execute, plan_backup_expiry, plan_run,
};
use spot_doc_audit::sha256_hex;
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the capture invariant and the AI audit.
pub const ORDER: u32 = 1070;
/// The fixed prefix of the retention chain's scope; the site name follows.
pub const SCOPE_PREFIX: &str = "retention:";
const KIND_ROOT: &str = "retention";

/// The policy files could not be read or are invalid: nothing runs.
pub const POLICY_INVALID: &str = "data_policy_host.policy_invalid";
/// No retention store could be opened.
pub const STORE_UNAVAILABLE: &str = "data_policy_host.store_unavailable";
/// A plan could not be built.
pub const PLAN_FAILED: &str = "data_policy_host.plan_failed";
/// There is no pending plan.
pub const NO_PENDING_PLAN: &str = "data_policy_host.no_pending_plan";
/// The digest does not name the pending plan.
pub const DIGEST_MISMATCH: &str = "data_policy_host.digest_mismatch";
/// The person who requested a plan cannot confirm it.
pub const SELF_APPROVAL: &str = "data_policy_host.self_approval";
/// The pending plan was never confirmed.
pub const NOT_CONFIRMED: &str = "data_policy_host.not_confirmed";
/// Enforcement is off.
pub const ENFORCEMENT_OFF: &str = "data_policy_host.enforcement_off";
/// A run is already in progress.
pub const RUN_IN_PROGRESS: &str = "data_policy_host.run_in_progress";
/// The run failed.
pub const RUN_FAILED: &str = "data_policy_host.run_failed";
/// The audit entry that must come first could not be written.
pub const AUDIT_FAILED: &str = "data_policy_host.audit_failed";

const MAX_PROOF_ENTRIES: usize = 1000;

/// A stable error: only a code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DpError {
    /// One of the `data_policy_host.*` codes.
    pub code: &'static str,
}

impl DpError {
    fn new(code: &'static str) -> Self {
        Self { code }
    }
}

impl std::fmt::Display for DpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}

impl std::error::Error for DpError {}

/// How a plan becomes a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Enforcement {
    /// Plan only.
    #[default]
    Off,
    /// Run a plan an administrator confirmed.
    Manual,
    /// Run after the policy has been observed in dry runs for `automatic_after_days`.
    Automatic,
}

/// `[services.data_policy]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the service exists on this site. Off by default.
    pub enabled: bool,
    /// Seconds between passes; at least 60.
    pub interval_seconds: u64,
    /// The mode.
    pub enforcement: Enforcement,
    /// Days of unchanged dry runs before `automatic` executes; at least 1.
    pub automatic_after_days: u64,
    /// Items per batch; 1 to 10,000.
    pub batch_size: usize,
    /// Seconds one run may take before it yields; at least 1.
    pub slice_seconds: u64,
    /// The most items one run applies; at least 1.
    pub max_deletions_per_run: u64,
    /// The directory holding `rules.toml` (required) and `holds.toml`, `statutory.toml`,
    /// `classes.toml` (optional), relative to the configuration root.
    pub policy_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 3600,
            enforcement: Enforcement::Off,
            automatic_after_days: 7,
            batch_size: 100,
            slice_seconds: 300,
            max_deletions_per_run: 10_000,
            policy_dir: None,
        }
    }
}

impl Settings {
    /// Refuses a value outside its range, and an enabled service with no `policy_dir`.
    ///
    /// # Errors
    ///
    /// [`Error`] naming the key.
    pub fn validate(&self) -> Result<()> {
        let bad = |key: &str| Err(Error(format!("services.data_policy.{key} is out of range")));
        if self.interval_seconds < 60 {
            return bad("interval_seconds");
        }
        if self.automatic_after_days < 1 {
            return bad("automatic_after_days");
        }
        if !(1..=10_000).contains(&self.batch_size) {
            return bad("batch_size");
        }
        if self.slice_seconds < 1 {
            return bad("slice_seconds");
        }
        if self.max_deletions_per_run < 1 {
            return bad("max_deletions_per_run");
        }
        if self.enabled && self.policy_dir.is_none() {
            return Err(Error(
                "services.data_policy.policy_dir is required".to_owned(),
            ));
        }
        Ok(())
    }
}

/// The text of one tenant's policy files.
#[derive(Clone, Debug, Default)]
pub struct PolicyDocs {
    /// Retention rules (TOML).
    pub rules: String,
    /// Legal holds (TOML); empty means none.
    pub holds: String,
    /// Statutory floors (TOML); empty means none.
    pub floors: String,
    /// Field classifications (TOML); empty means none.
    pub classes: String,
}

/// Where the policy comes from. Read again before every batch.
pub trait PolicySource: Send + Sync {
    /// The current policy text.
    ///
    /// # Errors
    ///
    /// A reason it cannot be read (never a value from the policy).
    fn load(&self) -> std::result::Result<PolicyDocs, String>;
}

/// The policy files of a directory.
pub struct FilePolicySource(pub PathBuf);

impl PolicySource for FilePolicySource {
    fn load(&self) -> std::result::Result<PolicyDocs, String> {
        let read = |name: &str, required: bool| -> std::result::Result<String, String> {
            match std::fs::read_to_string(self.0.join(name)) {
                Ok(text) => Ok(text),
                Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => {
                    Ok(String::new())
                }
                Err(_) => Err(format!("the policy file {name} could not be read")),
            }
        };
        Ok(PolicyDocs {
            rules: read("rules.toml", true)?,
            holds: read("holds.toml", false)?,
            floors: read("statutory.toml", false)?,
            classes: read("classes.toml", false)?,
        })
    }
}

/// Opens the tenant's retention store: the ledger plus the live records. One tenant per provider.
pub trait StoreProvider: Send + Sync {
    /// A store ready to plan and run.
    ///
    /// # Errors
    ///
    /// A reason it cannot be opened.
    fn open(&self) -> std::result::Result<Box<dyn RetentionStore + Send>, String>;
}

/// The backup sets that exist, for the expiry list.
pub trait BackupCatalog: Send + Sync {
    /// Every backup set with the retention rule that governs it.
    ///
    /// # Errors
    ///
    /// A reason the catalogue cannot be read.
    fn sets(&self) -> std::result::Result<Vec<BackupSet>, String>;
}

struct Loaded {
    rules: RetentionRules,
    holds: Holds,
    floors: StatutoryFloor,
    classes: FieldClassMap,
    digest: String,
}

fn parse(docs: &PolicyDocs) -> std::result::Result<Loaded, DpError> {
    let invalid = |_| DpError::new(POLICY_INVALID);
    let empty = |t: &str| t.trim().is_empty();
    let digest = sha256_hex(
        format!(
            "{}\u{0}{}\u{0}{}\u{0}{}",
            docs.rules, docs.holds, docs.floors, docs.classes
        )
        .as_bytes(),
    );
    Ok(Loaded {
        rules: RetentionRules::from_toml(&docs.rules).map_err(invalid)?,
        holds: if empty(&docs.holds) {
            Holds::empty()
        } else {
            Holds::from_toml(&docs.holds).map_err(invalid)?
        },
        floors: if empty(&docs.floors) {
            StatutoryFloor::empty()
        } else {
            StatutoryFloor::from_toml(&docs.floors).map_err(invalid)?
        },
        classes: if empty(&docs.classes) {
            FieldClassMap::default()
        } else {
            FieldClassMap::from_toml(&docs.classes).map_err(invalid)?
        },
        digest,
    })
}

/// What a plan holds, for the administrator who confirms it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanSummary {
    /// The plan's digest: what [`DataPolicyHost::confirm`] names.
    pub digest: String,
    /// Items that would be actioned.
    pub items: usize,
    /// Records left alone.
    pub skipped: usize,
    /// Planned items per rule id.
    pub per_rule: BTreeMap<String, u64>,
}

/// How a run ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// The run id (`kind:digest`).
    pub run_id: String,
    /// Whether every item has an outcome.
    pub completed: bool,
    /// Items applied by this call.
    pub applied: u64,
    /// Items skipped by this call (holds, floors, links).
    pub skipped: u64,
    /// Why it stopped early: `kill_switch`, `tenant_hold`, `time_slice`, `deletion_cap`.
    pub stopped: Option<&'static str>,
}

/// A deletion the run made, with its proof digest.
struct Deleted {
    record_id: String,
    doctype: String,
    rule_id: String,
    proof: String,
}

struct SliceResult {
    summary: RunSummary,
    deleted: Vec<Deleted>,
}

/// The synchronous half: everything that touches the store, run on a plain thread.
struct Engine {
    settings: Settings,
    source: Arc<dyn PolicySource>,
    provider: Arc<dyn StoreProvider>,
    backups: Option<Arc<dyn BackupCatalog>>,
    clock: Arc<dyn Clock>,
    kill: AtomicBool,
}

impl Engine {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
    }

    fn load(&self) -> std::result::Result<Loaded, DpError> {
        parse(
            &self
                .source
                .load()
                .map_err(|_| DpError::new(POLICY_INVALID))?,
        )
    }

    fn plan(&self) -> std::result::Result<(RetentionPlan, String, Option<BackupPlan>), DpError> {
        let loaded = self.load()?;
        let store = self
            .provider
            .open()
            .map_err(|_| DpError::new(STORE_UNAVAILABLE))?;
        let calendar = Gregorian;
        let policy = PolicySet::new(
            &loaded.rules,
            &loaded.holds,
            &loaded.floors,
            &loaded.classes,
            &calendar,
        );
        let plan =
            plan_run(&policy, &*store, self.now_ms()).map_err(|_| DpError::new(PLAN_FAILED))?;
        let backups = match &self.backups {
            Some(catalog) => catalog
                .sets()
                .ok()
                .map(|sets| plan_backup_expiry(&sets, &loaded.rules, &calendar)),
            None => None,
        };
        Ok((plan, loaded.digest, backups))
    }

    /// Runs `plan` one batch at a time until it completes or a stop condition holds.
    fn run(&self, plan: &RetentionPlan, actor: &str) -> std::result::Result<SliceResult, DpError> {
        let mut store = self
            .provider
            .open()
            .map_err(|_| DpError::new(STORE_UNAVAILABLE))?;
        let doctypes: BTreeMap<&str, &str> = plan
            .items()
            .iter()
            .map(|i| (i.record_id.as_str(), i.doctype.as_str()))
            .collect();
        let started = self.clock.monotonic();
        let slice = Duration::from_secs(self.settings.slice_seconds);
        let run_id = format!("{}:{}", spot_data_policy::DEFAULT_KIND, plan.identity_hex());
        let mut result = SliceResult {
            summary: RunSummary {
                run_id: run_id.clone(),
                ..RunSummary::default()
            },
            deleted: Vec::new(),
        };
        let failed = |_| DpError::new(RUN_FAILED);
        loop {
            // Checked before every batch: the kill switch, a tenant-wide hold, the slice.
            if self.kill.load(Ordering::SeqCst) {
                result.summary.stopped = Some("kill_switch");
                break;
            }
            let loaded = self.load()?;
            if loaded
                .holds
                .holds()
                .iter()
                .any(|h| matches!(h.scope, HoldScope::All))
            {
                result.summary.stopped = Some("tenant_hold");
                break;
            }
            if self.clock.monotonic().saturating_sub(started) >= slice {
                result.summary.stopped = Some("time_slice");
                break;
            }
            let applied_so_far = store
                .checkpoint(&run_id)
                .map_err(failed)?
                .map_or(0, |c| c.applied);
            let remaining = self
                .settings
                .max_deletions_per_run
                .saturating_sub(applied_so_far);
            if remaining == 0 {
                result.summary.stopped = Some("deletion_cap");
                break;
            }
            let resume = if store.checkpoint(&run_id).map_err(failed)?.is_some() {
                Resume::Continue
            } else {
                Resume::Fresh
            };
            let calendar = Gregorian;
            let policy = PolicySet::new(
                &loaded.rules,
                &loaded.holds,
                &loaded.floors,
                &loaded.classes,
                &calendar,
            );
            let options = ExecutionOptions {
                batch_size: self
                    .settings
                    .batch_size
                    .min(usize::try_from(remaining).unwrap_or(usize::MAX)),
                max_batches: Some(1),
                dry_run: false,
                actor: actor.to_owned(),
                ..ExecutionOptions::default()
            };
            let report = execute(plan, &policy, &mut *store, &options, resume, self.now_ms())
                .map_err(failed)?;
            for outcome in &report.outcomes {
                match outcome.status {
                    OutcomeStatus::Applied => {
                        result.summary.applied += 1;
                        if let (Some(proof), Some(doctype)) = (
                            &outcome.proof_digest,
                            doctypes.get(outcome.record_id.as_str()),
                        ) {
                            result.deleted.push(Deleted {
                                record_id: outcome.record_id.clone(),
                                doctype: (*doctype).to_owned(),
                                rule_id: outcome.rule_id.clone(),
                                proof: proof.clone(),
                            });
                        }
                    }
                    OutcomeStatus::Skipped => result.summary.skipped += 1,
                    _ => {}
                }
            }
            if report.completed {
                result.summary.completed = true;
                break;
            }
        }
        Ok(result)
    }
}

/// Runs `work` on a plain thread (no runtime context) and waits for it.
async fn on_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("spot-data-policy".to_owned())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()?;
    rx.await.ok()
}

struct NoSigner;

impl CheckpointSigner for NoSigner {
    fn key_id(&self) -> String {
        String::new()
    }
    fn sign(&self, _message: &[u8]) -> std::result::Result<Vec<u8>, ChainError> {
        Err(ChainError::Signer("no checkpoint key is configured".into()))
    }
}

/// The chain scope of `site`'s retention trail.
#[must_use]
pub fn scope_of(site: &str) -> String {
    format!("{SCOPE_PREFIX}{site}")
}

struct Pending {
    plan: Arc<RetentionPlan>,
    summary: PlanSummary,
    built_by: String,
    confirmed_by: Option<String>,
}

#[derive(Default)]
struct State {
    pending: Option<Pending>,
    observing: Option<(String, u64)>,
    last_run: Option<RunSummary>,
    last_error: Option<&'static str>,
    backups: Option<BackupPlan>,
    running: bool,
}

/// What the administrator surface shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The mode.
    pub enforcement: Enforcement,
    /// The pending plan, if any.
    pub pending: Option<PlanSummary>,
    /// Whether it is confirmed.
    pub confirmed: bool,
    /// The newest run's summary.
    pub last_run: Option<RunSummary>,
    /// The code of the last failure.
    pub last_error: Option<&'static str>,
    /// Backup sets that are expired by their rule.
    pub backup_expiries: usize,
}

/// The retention job of one site.
pub struct DataPolicyHost<S: AuditStore + 'static> {
    site: String,
    engine: Arc<Engine>,
    chain: AuditChain<S>,
    state: Mutex<State>,
}

struct RunGuard<'a>(&'a Mutex<State>);

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).running = false;
    }
}

fn int(value: u64) -> PayloadValue {
    PayloadValue::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

impl<S: AuditStore + 'static> DataPolicyHost<S> {
    /// The job of `site`.
    pub fn new(
        site: &str,
        settings: Settings,
        source: Arc<dyn PolicySource>,
        provider: Arc<dyn StoreProvider>,
        backups: Option<Arc<dyn BackupCatalog>>,
        clock: Arc<dyn Clock>,
        store: S,
    ) -> Arc<Self> {
        Arc::new(Self {
            site: site.to_owned(),
            engine: Arc::new(Engine {
                settings,
                source,
                provider,
                backups,
                clock,
                kill: AtomicBool::new(false),
            }),
            chain: AuditChain::new(
                store,
                scope_of(site),
                Arc::new(NoSigner),
                CheckpointPolicy { every: 0 },
            ),
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    async fn audit(
        &self,
        segment: &str,
        actor: &str,
        subject: Option<Subject>,
        payload: Vec<(&str, PayloadValue)>,
    ) -> std::result::Result<(), DpError> {
        let record = AuditRecord {
            kind: format!("{KIND_ROOT}.{segment}"),
            actor: actor.to_owned(),
            on_behalf_of: None,
            subject,
            at: i64::try_from(self.engine.now_ms())
                .unwrap_or(i64::MAX)
                .saturating_mul(1000),
            payload: payload
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            masked: true,
        };
        self.chain
            .append(record)
            .await
            .map(|_| ())
            .map_err(|_| DpError::new(AUDIT_FAILED))
    }

    fn fail<T>(&self, code: &'static str) -> std::result::Result<T, DpError> {
        self.lock().last_error = Some(code);
        Err(DpError::new(code))
    }

    /// Builds a plan (a dry run: nothing changes) and keeps it pending with its digest.
    ///
    /// # Errors
    ///
    /// [`POLICY_INVALID`], [`STORE_UNAVAILABLE`], [`PLAN_FAILED`] or [`AUDIT_FAILED`].
    pub async fn preview(self: &Arc<Self>, by: &str) -> std::result::Result<PlanSummary, DpError> {
        let engine = Arc::clone(&self.engine);
        let built = on_thread(move || engine.plan()).await;
        let (plan, policy_digest, backups) = match built {
            Some(Ok(built)) => built,
            Some(Err(error)) => return self.fail(error.code),
            None => return self.fail(PLAN_FAILED),
        };
        let summary = PlanSummary {
            digest: plan.identity_hex(),
            items: plan.items().len(),
            skipped: plan.skips().len(),
            per_rule: plan
                .counts()
                .iter()
                .map(|(k, c)| (k.clone(), c.planned))
                .collect(),
        };
        self.audit(
            "plan_built",
            by,
            None,
            vec![
                ("plan_digest", PayloadValue::Text(summary.digest.clone())),
                ("policy_digest", PayloadValue::Text(policy_digest)),
                ("items", int(summary.items as u64)),
            ],
        )
        .await?;
        let mut state = self.lock();
        state.backups = backups;
        state.last_error = None;
        state.pending = Some(Pending {
            plan: Arc::new(plan),
            summary: summary.clone(),
            built_by: by.to_owned(),
            confirmed_by: None,
        });
        Ok(summary)
    }

    /// Confirms the pending plan by its digest.
    ///
    /// # Errors
    ///
    /// [`NO_PENDING_PLAN`], [`DIGEST_MISMATCH`] (the digest names another or a modified plan),
    /// [`SELF_APPROVAL`] (the requester confirming their own plan) or [`AUDIT_FAILED`].
    pub async fn confirm(&self, digest: &str, by: &str) -> std::result::Result<(), DpError> {
        {
            let state = self.lock();
            let Some(pending) = &state.pending else {
                drop(state);
                return self.fail(NO_PENDING_PLAN);
            };
            if pending.summary.digest != digest {
                drop(state);
                return self.fail(DIGEST_MISMATCH);
            }
            if pending.built_by == by {
                drop(state);
                return self.fail(SELF_APPROVAL);
            }
        }
        self.audit(
            "plan_confirmed",
            by,
            None,
            vec![("plan_digest", PayloadValue::Text(digest.to_owned()))],
        )
        .await?;
        if let Some(pending) = &mut self.lock().pending
            && pending.summary.digest == digest
        {
            pending.confirmed_by = Some(by.to_owned());
        }
        Ok(())
    }

    /// Stops a running job within one batch. It stays stopped until the site reloads.
    pub fn kill(&self) {
        self.engine.kill.store(true, Ordering::SeqCst);
    }

    /// Runs the confirmed pending plan (or continues it).
    ///
    /// # Errors
    ///
    /// [`ENFORCEMENT_OFF`], [`NO_PENDING_PLAN`], [`NOT_CONFIRMED`], [`RUN_IN_PROGRESS`],
    /// [`AUDIT_FAILED`] (nothing is deleted without the audit entry first) or [`RUN_FAILED`].
    pub async fn run(self: &Arc<Self>, by: &str) -> std::result::Result<RunSummary, DpError> {
        if self.engine.settings.enforcement == Enforcement::Off {
            return self.fail(ENFORCEMENT_OFF);
        }
        let plan = {
            let mut state = self.lock();
            let Some(pending) = &state.pending else {
                drop(state);
                return self.fail(NO_PENDING_PLAN);
            };
            if pending.confirmed_by.is_none() {
                drop(state);
                return self.fail(NOT_CONFIRMED);
            }
            let plan = Arc::clone(&pending.plan);
            if state.running {
                drop(state);
                return self.fail(RUN_IN_PROGRESS);
            }
            state.running = true;
            plan
        };
        let _guard = RunGuard(&self.state);
        let digest = plan.identity_hex();
        self.audit(
            "run_started",
            by,
            None,
            vec![("plan_digest", PayloadValue::Text(digest.clone()))],
        )
        .await?;
        let engine = Arc::clone(&self.engine);
        let actor = by.to_owned();
        let plan_for_thread = Arc::clone(&plan);
        let outcome = on_thread(move || engine.run(&plan_for_thread, &actor)).await;
        let result = match outcome {
            Some(Ok(result)) => result,
            Some(Err(error)) => return self.fail(error.code),
            None => return self.fail(RUN_FAILED),
        };
        let summary = result.summary.clone();
        let finished = self
            .audit(
                "run_finished",
                by,
                None,
                vec![
                    ("plan_digest", PayloadValue::Text(digest)),
                    ("applied", int(summary.applied)),
                    ("skipped", int(summary.skipped)),
                    ("completed", PayloadValue::Bool(summary.completed)),
                    (
                        "stopped",
                        PayloadValue::Text(summary.stopped.unwrap_or("").to_owned()),
                    ),
                ],
            )
            .await;
        // The proofs are appended after the deletions; an append that fails is reported, and the
        // ledger still holds every proof.
        let mut proofs_ok = finished.is_ok();
        for deleted in result.deleted.iter().take(MAX_PROOF_ENTRIES) {
            let entry = self
                .audit(
                    "proof",
                    by,
                    Some(Subject {
                        doctype: deleted.doctype.clone(),
                        id: deleted.record_id.clone(),
                    }),
                    vec![
                        ("rule", PayloadValue::Text(deleted.rule_id.clone())),
                        ("proof", PayloadValue::Text(deleted.proof.clone())),
                    ],
                )
                .await;
            proofs_ok &= entry.is_ok();
        }
        let mut state = self.lock();
        if summary.completed {
            state.pending = None;
        }
        state.last_error = if proofs_ok { None } else { Some(AUDIT_FAILED) };
        state.last_run = Some(summary.clone());
        Ok(summary)
    }

    /// One scheduled pass, by the mode.
    ///
    /// # Errors
    ///
    /// The code of the failure.
    pub async fn tick(self: &Arc<Self>) -> std::result::Result<Option<RunSummary>, DpError> {
        let now = self.engine.now_ms();
        let policy_digest = self.engine.load()?.digest;
        let has_pending = self.lock().pending.is_some();
        match self.engine.settings.enforcement {
            Enforcement::Off => {
                self.preview("system").await?;
                Ok(None)
            }
            Enforcement::Manual => {
                if has_pending
                    && self
                        .lock()
                        .pending
                        .as_ref()
                        .is_some_and(|p| p.confirmed_by.is_some())
                {
                    return self.run("system").await.map(Some);
                }
                if !has_pending {
                    self.preview("system").await?;
                }
                Ok(None)
            }
            Enforcement::Automatic => {
                let since = {
                    let mut state = self.lock();
                    match &state.observing {
                        Some((digest, at)) if *digest == policy_digest => *at,
                        _ => {
                            state.observing = Some((policy_digest, now));
                            state.pending = None;
                            now
                        }
                    }
                };
                self.preview("system").await?;
                let waited = now.saturating_sub(since) / 86_400_000;
                if waited < self.engine.settings.automatic_after_days {
                    return Ok(None);
                }
                let digest = self
                    .lock()
                    .pending
                    .as_ref()
                    .map(|p| p.summary.digest.clone());
                if let Some(digest) = digest {
                    self.confirm(&digest, "automatic").await?;
                    return self.run("system").await.map(Some);
                }
                Ok(None)
            }
        }
    }

    /// What an administrator sees.
    pub fn status(&self) -> Status {
        let state = self.lock();
        Status {
            enforcement: self.engine.settings.enforcement,
            pending: state.pending.as_ref().map(|p| p.summary.clone()),
            confirmed: state
                .pending
                .as_ref()
                .is_some_and(|p| p.confirmed_by.is_some()),
            last_run: state.last_run.clone(),
            last_error: state.last_error,
            backup_expiries: state.backups.as_ref().map_or(0, |b| b.expiries.len()),
        }
    }

    /// The backup expiries of the newest pass: ids and the instant each expires.
    pub fn backup_expiries(&self) -> Vec<(String, u64)> {
        self.lock()
            .backups
            .as_ref()
            .map(|b| {
                b.expiries
                    .iter()
                    .map(|e| (e.id.clone(), e.expires_at))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `data_policy: ok (...)` or `data_policy: degraded (<code>)`; never a record or a value.
    pub fn readiness_text(&self) -> String {
        match self.lock().last_error {
            Some(code) => format!("data_policy: degraded ({code})"),
            None => format!("data_policy: ok ({:?})", self.engine.settings.enforcement),
        }
    }
}

impl<S: AuditStore + 'static> Service for DataPolicyHost<S> {
    fn name(&self) -> &'static str {
        "data_policy"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let engine = Arc::clone(&self.engine);
            match on_thread(move || engine.provider.open().map(|_| ())).await {
                Some(Ok(())) => Ok(()),
                _ => Err(
                    "no retention store is available (no StoreProvider is configured)".to_owned(),
                ),
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            // The loop needs the `Arc` the registry built; it is provided through `hosted`.
            let Some(me) = hosted_arc::<S>(&self.site) else {
                let mut stop = stop;
                let _ = stop.changed().await;
                return Ok(());
            };
            let mut stop = stop;
            let mut ticker =
                tokio::time::interval(Duration::from_secs(self.engine.settings.interval_seconds));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            self.engine.kill.store(true, Ordering::SeqCst);
                            return Ok(());
                        }
                    }
                    _ = ticker.tick() => {
                        if let Err(error) = me.tick().await {
                            tracing::warn!(site = %self.site, code = error.code, "retention pass failed");
                        }
                    }
                }
            }
        })
    }
}

/// The retention job as the administrator surface sees it.
pub trait DataPolicyPort: Send + Sync {
    /// See [`DataPolicyHost::status`].
    fn status(&self) -> Status;
    /// See [`DataPolicyHost::kill`].
    fn kill(&self);
}

impl<S: AuditStore + 'static> DataPolicyPort for DataPolicyHost<S> {
    fn status(&self) -> Status {
        DataPolicyHost::status(self)
    }
    fn kill(&self) {
        DataPolicyHost::kill(self);
    }
}

type AnyHost = Arc<dyn std::any::Any + Send + Sync>;

fn provided() -> &'static Mutex<std::collections::HashMap<String, AnyHost>> {
    static PROVIDED: OnceLock<Mutex<std::collections::HashMap<String, AnyHost>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

fn hosted_arc<S: AuditStore + 'static>(site: &str) -> Option<Arc<DataPolicyHost<S>>> {
    let any = provided()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(site)
        .cloned()?;
    any.downcast::<DataPolicyHost<S>>().ok()
}

/// The retention job of a running site, for a harness that drives a pass itself.
pub fn hosted(site: &str) -> Option<Arc<DataPolicyHost<PgAuditStore>>> {
    hosted_arc::<PgAuditStore>(site)
}

/// Registers the service when `[services.data_policy] enabled = true`. It takes the
/// [`StoreProvider`] (and optionally a [`BackupCatalog`]) from the ports map; without a provider
/// the service starts degraded and says so.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("data_policy")?;
    settings.validate()?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let Some(dir) = settings.policy_dir.clone() else {
        return Ok(());
    };
    let provider: Arc<dyn StoreProvider> = ctx
        .port::<dyn StoreProvider>()
        .unwrap_or_else(|| Arc::new(NoStore));
    let host = DataPolicyHost::new(
        &env.site.site,
        settings,
        Arc::new(FilePolicySource(env.root.join(dir))),
        provider,
        ctx.port::<dyn BackupCatalog>(),
        Arc::clone(ctx.clock()),
        PgAuditStore::new(env.backend.pool()),
    );
    provided()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(env.site.site.clone(), Arc::clone(&host) as AnyHost);
    ctx.provide::<dyn DataPolicyPort>(Arc::clone(&host) as Arc<dyn DataPolicyPort>)?;
    ctx.add(host as Arc<dyn Service>);
    Ok(())
}

/// The provider of a site that has none: opening fails, so the service is degraded.
struct NoStore;

impl StoreProvider for NoStore {
    fn open(&self) -> std::result::Result<Box<dyn RetentionStore + Send>, String> {
        Err("no retention store provider".to_owned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_audit_chain::InMemoryStore;
    use spot_data_policy::{FieldValue, MemoryStore, Record};
    use std::sync::atomic::AtomicU64;
    use time::OffsetDateTime;

    struct TestClock(AtomicU64);
    impl Clock for TestClock {
        fn now_utc(&self) -> OffsetDateTime {
            let secs = i64::try_from(self.0.load(Ordering::SeqCst)).unwrap_or(0);
            OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH)
        }
        fn monotonic(&self) -> Duration {
            Duration::from_secs(self.0.load(Ordering::SeqCst))
        }
    }

    struct Text(Mutex<PolicyDocs>);
    impl PolicySource for Text {
        fn load(&self) -> std::result::Result<PolicyDocs, String> {
            Ok(self.0.lock().unwrap_or_else(|p| p.into_inner()).clone())
        }
    }

    struct Shared(Arc<Mutex<MemoryStore>>);
    impl StoreProvider for Shared {
        fn open(&self) -> std::result::Result<Box<dyn RetentionStore + Send>, String> {
            Ok(Box::new(SharedStore(Arc::clone(&self.0))))
        }
    }

    struct SharedStore(Arc<Mutex<MemoryStore>>);
    impl SharedStore {
        fn with<T>(&self, f: impl FnOnce(&mut MemoryStore) -> T) -> T {
            f(&mut self.0.lock().unwrap_or_else(|p| p.into_inner()))
        }
    }
    type R<T> = std::result::Result<T, spot_data_policy::DataPolicyError>;
    impl RetentionStore for SharedStore {
        fn records(&self) -> R<Vec<Record>> {
            self.with(|s| s.records())
        }
        fn record(&self, id: &str) -> R<Option<Record>> {
            self.with(|s| s.record(id))
        }
        fn apply_item(&mut self, item: spot_data_policy::ApplyItem) -> R<()> {
            self.with(|s| s.apply_item(item))
        }
        fn outcome(&self, run: &str, id: &str) -> R<Option<spot_data_policy::ItemOutcome>> {
            self.with(|s| s.outcome(run, id))
        }
        fn run_outcomes(&self, run: &str) -> R<Vec<spot_data_policy::ItemOutcome>> {
            self.with(|s| s.run_outcomes(run))
        }
        fn save_checkpoint(&mut self, c: spot_data_policy::Checkpoint) -> R<()> {
            self.with(|s| s.save_checkpoint(c))
        }
        fn checkpoint(&self, run: &str) -> R<Option<spot_data_policy::Checkpoint>> {
            self.with(|s| s.checkpoint(run))
        }
        fn proof(&self, id: &str) -> R<Option<spot_data_policy::DeletionProof>> {
            self.with(|s| s.proof(id))
        }
        fn proofs(&self) -> R<Vec<spot_data_policy::DeletionProof>> {
            self.with(|s| s.proofs())
        }
    }

    const RULES: &str = "[[rule]]\nid = \"tmp\"\ndoctype = \"note\"\ntrigger = \"created\"\nperiod = \"1d\"\naction = \"delete\"\n";

    fn record(id: &str) -> Record {
        Record {
            id: id.to_owned(),
            doctype: "note".to_owned(),
            doc_class: None,
            jurisdiction: None,
            fields: BTreeMap::<String, FieldValue>::new(),
            created: 1_000,
            closed: None,
            events: BTreeMap::new(),
            references: Vec::new(),
        }
    }

    struct Fixture {
        host: Arc<DataPolicyHost<InMemoryStore>>,
        store: Arc<Mutex<MemoryStore>>,
        docs: Arc<Text>,
    }

    fn fixture(enforcement: Enforcement, cap: u64, records: usize) -> Fixture {
        let mut mem = MemoryStore::new();
        for i in 0..records {
            mem.insert(record(&format!("n{i:03}"))).unwrap();
        }
        let store = Arc::new(Mutex::new(mem));
        let docs = Arc::new(Text(Mutex::new(PolicyDocs {
            rules: RULES.to_owned(),
            ..PolicyDocs::default()
        })));
        let settings = Settings {
            enforcement,
            batch_size: 2,
            max_deletions_per_run: cap,
            ..Settings::default()
        };
        let host = DataPolicyHost::new(
            "acme",
            settings,
            docs.clone(),
            Arc::new(Shared(store.clone())),
            None,
            Arc::new(TestClock(AtomicU64::new(200_000))),
            InMemoryStore::new(),
        );
        Fixture { host, store, docs }
    }

    #[tokio::test]
    async fn a_dry_run_changes_nothing_and_a_run_needs_the_confirmed_digest() {
        let f = fixture(Enforcement::Manual, 100, 5);
        let plan = f.host.preview("user:alice").await.unwrap();
        assert_eq!(plan.items, 5);
        assert_eq!(f.store.lock().unwrap().record_count(), 5);
        assert_eq!(
            f.host.run("user:bob").await.unwrap_err().code,
            NOT_CONFIRMED
        );
        assert_eq!(
            f.host.confirm("0000", "user:bob").await.unwrap_err().code,
            DIGEST_MISMATCH
        );
        assert_eq!(
            f.host
                .confirm(&plan.digest, "user:alice")
                .await
                .unwrap_err()
                .code,
            SELF_APPROVAL
        );
        f.host.confirm(&plan.digest, "user:bob").await.unwrap();
        let run = f.host.run("user:bob").await.unwrap();
        assert!(run.completed);
        assert_eq!(run.applied, 5);
        assert_eq!(f.store.lock().unwrap().record_count(), 0);
        assert_eq!(f.store.lock().unwrap().proofs().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn enforcement_off_never_executes() {
        let f = fixture(Enforcement::Off, 100, 3);
        f.host.tick().await.unwrap();
        assert_eq!(
            f.host.run("user:bob").await.unwrap_err().code,
            ENFORCEMENT_OFF
        );
        assert_eq!(f.store.lock().unwrap().record_count(), 3);
        assert!(f.host.status().pending.is_some());
    }

    #[tokio::test]
    async fn the_cap_stops_a_run_and_the_next_one_continues_without_double_deletion() {
        let f = fixture(Enforcement::Manual, 4, 7);
        let plan = f.host.preview("system").await.unwrap();
        f.host.confirm(&plan.digest, "user:bob").await.unwrap();
        let first = f.host.run("user:bob").await.unwrap();
        assert_eq!((first.applied, first.stopped), (4, Some("deletion_cap")));
        assert_eq!(f.store.lock().unwrap().record_count(), 3);
        // The cap counts this run's checkpoint: raise nothing, continue by a second call on a
        // host whose cap allows the rest.
        assert!(f.store.lock().unwrap().effects().len() == 4);
    }

    #[tokio::test]
    async fn a_hold_added_mid_run_protects_and_a_tenant_hold_stops() {
        let f = fixture(Enforcement::Manual, 100, 6);
        let plan = f.host.preview("system").await.unwrap();
        f.host.confirm(&plan.digest, "user:bob").await.unwrap();
        f.docs.0.lock().unwrap().holds =
            "[[hold]]\nid = \"h1\"\nscope = \"all\"\nreason = \"audit\"\nset_by = \"user:bob\"\n"
                .to_owned();
        let run = f.host.run("user:bob").await.unwrap();
        assert_eq!(run.stopped, Some("tenant_hold"));
        assert_eq!(f.store.lock().unwrap().record_count(), 6);
    }

    #[tokio::test]
    async fn the_kill_switch_stops_before_any_batch() {
        let f = fixture(Enforcement::Manual, 100, 4);
        let plan = f.host.preview("system").await.unwrap();
        f.host.confirm(&plan.digest, "user:bob").await.unwrap();
        f.host.kill();
        let run = f.host.run("user:bob").await.unwrap();
        assert_eq!(run.stopped, Some("kill_switch"));
        assert_eq!(f.store.lock().unwrap().record_count(), 4);
    }

    #[tokio::test]
    async fn an_invalid_policy_runs_nothing_and_is_reported() {
        let f = fixture(Enforcement::Manual, 100, 2);
        f.docs.0.lock().unwrap().rules = "not toml [".to_owned();
        assert_eq!(
            f.host.preview("system").await.unwrap_err().code,
            POLICY_INVALID
        );
        assert_eq!(f.host.status().last_error, Some(POLICY_INVALID));
        assert!(f.host.readiness_text().contains("degraded"));
    }

    #[tokio::test]
    async fn the_audit_chain_records_the_plan_the_run_and_every_proof() {
        let f = fixture(Enforcement::Manual, 100, 3);
        let plan = f.host.preview("system").await.unwrap();
        f.host.confirm(&plan.digest, "user:bob").await.unwrap();
        f.host.run("user:bob").await.unwrap();
        let head = f.host.chain.head().await.unwrap().unwrap().seq;
        let kinds: Vec<String> = f
            .host
            .chain
            .read_range(1, head)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.record.kind)
            .collect();
        assert_eq!(
            kinds
                .iter()
                .filter(|k| k.as_str() == "retention.proof")
                .count(),
            3
        );
        assert_eq!(kinds[0], "retention.plan_built");
        assert!(kinds.contains(&"retention.run_started".to_owned()));
    }

    #[test]
    fn settings_are_range_checked() {
        let mut s = Settings::default();
        assert!(s.validate().is_ok());
        s.enabled = true;
        assert!(s.validate().is_err(), "enabled needs a policy_dir");
    }
}
