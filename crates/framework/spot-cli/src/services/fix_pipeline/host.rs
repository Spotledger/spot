//! The hosted fix pipeline: one engine run per site at a time, journaled in PostgreSQL, with the
//! effect hold on between the first live change and verification.
//!
//! # A run
//!
//! [`FixHost::apply_remedy`] is what the remedy host calls (through [`PipelinePath`], the
//! [`ApplyPath`] it installs) for every remedy above T1 or not reversible:
//!
//! 1. the remedy's plan is normalised into a sealed `FixPlan` and a fix id is derived;
//! 2. **one run per site**: a second concurrent run is refused, not queued;
//! 3. the engine runs on its own plain thread (see [`link`](super::link)): the journal records
//!    `Proposed`, `Planned`; the plan is **rehearsed on the rehearsal snapshot, never on the live
//!    site**; a T3 plan is approved (a deep agent may self-approve, nobody else can here);
//! 4. the apply journals `ApplyStarted`, takes a snapshot for a T3 plan, captures the pre-images
//!    (state fingerprints of every affected target) and the pre-state fingerprint, **engages the
//!    effect hold**, then applies the remedy live and journals every change;
//! 5. verification re-reads every changed target and runs the site's checks. A failure runs the
//!    remedy's undo, re-reads the targets and compares the post-rollback fingerprint with the
//!    pre-state: only an empty difference is `RolledBack`, anything else is `Failed`, never a
//!    claim that the site is back;
//! 6. after the engine ends the hold is **released** if the fix verified (held effects replay in
//!    order with their original keys) and **discarded** if it did not.
//!
//! # What survives a restart
//!
//! The journal and the audit records do. The remedy framework's undo log does not (it is in
//! memory), so after a restart [`FixHost::maintain`] can finish a verification and close an expired
//! undo window, but a fix that was in flight when the process ended cannot be reversed by its
//! remedy: it ends `Failed`, with evidence, for an operator. That is stated, not hidden.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use spot_agent_tiers::Tier;
use spot_core::Clock;
use spot_effect_hold::{HoldService, ReleasePlan};
use spot_fix_pipeline::{
    AgentRef, DeploymentEpoch, FixContext, FixEngine, FixError, FixId, FixJournal, FixOutcome,
    FixPlan, FixPolicy, FixPorts, FixState, NoFaults, Progress, SnapshotRef,
};
use spot_mcp::AuditSink;
use spot_remedies::{Applied, RemedyError, RemedyId};

use super::link::{Link, Side, serve};
use super::ports::{
    DigestPreImages, FixChecks, LinkAudit, LinkSnapshots, NoApproval, Probe, RemedyExecutor,
    SnapshotRehearsal, StateVerifier,
};
use crate::services::remedies::host::{ApplyPath, Fut, HostError, PipelineRun, RemedyHost};

/// The effect hold the pipeline engages between the first live change and verification.
pub trait EffectHold: Send + Sync {
    /// Starts holding external effects under the migration id `id`.
    fn begin<'a>(&'a self, id: &'a str, reason: &'a str, now: i64) -> Fut<'a, Result<(), String>>;
    /// Releases the hold: the held effects replay in order with their original keys.
    fn release<'a>(&'a self, id: &'a str, now: i64) -> Fut<'a, Result<(), String>>;
    /// Discards the hold: the held effects are never sent.
    fn discard<'a>(&'a self, id: &'a str, reason: &'a str, now: i64)
    -> Fut<'a, Result<(), String>>;
}

/// The effect hold service of the site as an [`EffectHold`].
pub struct HoldAdapter(pub Arc<HoldService>);

impl EffectHold for HoldAdapter {
    fn begin<'a>(&'a self, id: &'a str, reason: &'a str, now: i64) -> Fut<'a, Result<(), String>> {
        Box::pin(async move {
            self.0
                .begin(id, reason, now, None)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    fn release<'a>(&'a self, id: &'a str, now: i64) -> Fut<'a, Result<(), String>> {
        Box::pin(async move {
            let report = self
                .0
                .release(id, ReleasePlan::new(), now)
                .await
                .map_err(|e| e.to_string())?;
            if report.completed {
                Ok(())
            } else {
                Err("the release left held effects behind".to_owned())
            }
        })
    }

    fn discard<'a>(
        &'a self,
        id: &'a str,
        reason: &'a str,
        now: i64,
    ) -> Fut<'a, Result<(), String>> {
        Box::pin(async move {
            self.0
                .discard(id, reason, now)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}

/// Takes and restores site snapshots for a structural (T3) plan.
pub trait Snapshots: Send + Sync {
    /// Takes a snapshot for `fix`.
    fn take<'a>(&'a self, fix: &'a str) -> Fut<'a, Result<SnapshotRef, String>>;
    /// Restores `snapshot`; safe to call again.
    fn restore<'a>(&'a self, snapshot: &'a SnapshotRef) -> Fut<'a, Result<(), String>>;
}

/// No snapshot provider: a plan that needs a snapshot fails at the snapshot step, before anything
/// is changed.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSnapshots;

impl Snapshots for NoSnapshots {
    fn take<'a>(&'a self, _fix: &'a str) -> Fut<'a, Result<SnapshotRef, String>> {
        Box::pin(async { Err("no snapshot provider is installed".to_owned()) })
    }

    fn restore<'a>(&'a self, _snapshot: &'a SnapshotRef) -> Fut<'a, Result<(), String>> {
        Box::pin(async { Err("no snapshot provider is installed".to_owned()) })
    }
}

/// What the host is built from.
pub struct Deps {
    /// The site (the only tenant this host serves).
    pub site: String,
    /// The remedy host: plan source, world, undo by token.
    pub remedies: Arc<RemedyHost>,
    /// The write-ahead change journal (PostgreSQL in a running site).
    pub journal: Arc<dyn FixJournal>,
    /// The audit chain.
    pub sink: Arc<dyn AuditSink>,
    /// The effect hold, when installed.
    pub hold: Option<Arc<dyn EffectHold>>,
    /// Whether a fix must not apply without the hold.
    pub require_hold: bool,
    /// The snapshot provider.
    pub snapshots: Arc<dyn Snapshots>,
    /// The site's post-apply checks.
    pub checks: Arc<dyn FixChecks>,
    /// The pipeline policy (undo window, rehearsal, deep-agent approval).
    pub policy: FixPolicy,
    /// The deployment epoch the audit records carry.
    pub epoch: u64,
    /// The injected clock.
    pub clock: Arc<dyn Clock>,
}

/// What a verified fix left, for a later undo.
#[derive(Clone)]
struct Record {
    remedy: RemedyId,
    agent_id: String,
    applied: Applied,
}

/// What one maintenance pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Maintenance {
    /// Unfinished fixes the journal showed, and recovery was run on.
    pub recovered: u64,
    /// Of those, how many ended `Failed` or errored.
    pub failed: u64,
    /// Verified fixes whose undo window was closed by expiry.
    pub closed: u64,
}

/// The hosted pipeline of one site.
pub struct FixHost {
    deps: Deps,
    run_lock: tokio::sync::Mutex<()>,
    seq: AtomicU64,
    records: Mutex<HashMap<String, Record>>,
    degraded: Mutex<Option<String>>,
}

fn op_failed(detail: impl Into<String>) -> HostError {
    HostError::Remedy(RemedyError::OpFailed {
        operation: "fix_pipeline".to_owned(),
        detail: detail.into(),
    })
}

/// The inputs of one engine run, all owned (the thread outlives no borrow).
struct Job {
    journal: Arc<dyn FixJournal>,
    policy: FixPolicy,
    probe: Probe,
    targets: Vec<String>,
    checks: Arc<dyn FixChecks>,
    applied: Option<Applied>,
    can_apply: bool,
}

impl Job {
    /// Builds the engine over the ports and hands it to `work`; returns its result and whatever
    /// the executor recorded.
    fn execute<T>(
        self,
        link: Link,
        work: impl FnOnce(&mut FixEngine<'_>) -> T,
    ) -> (T, Option<Applied>) {
        let mut pre_images = DigestPreImages {
            probe: self.probe.clone(),
        };
        let mut snapshots = LinkSnapshots { link: link.clone() };
        let mut executor = RemedyExecutor::new(link.clone(), self.can_apply);
        executor.applied = self.applied.clone();
        let mut verifier = StateVerifier {
            probe: self.probe.clone(),
            targets: self.targets.clone(),
            checks: Arc::clone(&self.checks),
        };
        let mut rehearsal = SnapshotRehearsal { link: link.clone() };
        let mut approval = NoApproval;
        let mut audit = LinkAudit { link };
        let faults = NoFaults;
        let out = {
            let mut engine = FixEngine::new(
                self.policy.clone(),
                &*self.journal,
                &mut audit,
                FixPorts {
                    pre_images: &mut pre_images,
                    snapshots: &mut snapshots,
                    executor: &mut executor,
                    verifier: &mut verifier,
                    rehearsal: &mut rehearsal,
                    approval: &mut approval,
                },
                &faults,
            );
            work(&mut engine)
        };
        (out, executor.applied)
    }
}

impl FixHost {
    /// A host over `deps`.
    #[must_use]
    pub fn new(deps: Deps) -> Self {
        let start = u64::try_from(deps.clock.now_utc().unix_timestamp()).unwrap_or(0);
        Self {
            deps,
            run_lock: tokio::sync::Mutex::new(()),
            seq: AtomicU64::new(start.saturating_mul(1_000)),
            records: Mutex::new(HashMap::new()),
            degraded: Mutex::new(None),
        }
    }

    fn now(&self) -> i64 {
        self.deps.clock.now_utc().unix_timestamp()
    }

    fn probe(&self) -> Option<Probe> {
        self.deps
            .remedies
            .world()
            .map(|world| Probe::new(world, self.deps.remedies.settings().clone(), self.now()))
    }

    /// Why the host is degraded right now (a hold that could not be released), if it is.
    #[must_use]
    pub fn degraded(&self) -> Option<String> {
        self.degraded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn degrade(&self, reason: &str) {
        *self.degraded.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason.to_owned());
    }

    fn side<'a>(
        &'a self,
        direct: Option<&'a crate::services::remedies::host::DirectApply<'a>>,
        tenant: &'a str,
        agent_id: &'a str,
        remedy: Option<&'a RemedyId>,
        fix: &FixId,
    ) -> Side<'a> {
        Side {
            direct,
            remedies: &self.deps.remedies,
            tenant,
            agent_id,
            remedy,
            sink: &self.deps.sink,
            hold: self.deps.hold.as_ref(),
            require_hold: self.deps.require_hold,
            snapshots: &self.deps.snapshots,
            held_as: None,
            fix: fix.to_string(),
            now: self.now(),
        }
    }

    /// Runs one remedy plan through the pipeline: the body of the apply path.
    ///
    /// # Errors
    ///
    /// A [`HostError`]: no world, the plan cannot be normalised, another fix is running, or the
    /// fix ended anywhere but `Verified` (the error says how, and the site is as it was).
    pub async fn apply_remedy(&self, run: PipelineRun<'_>) -> Result<Applied, HostError> {
        let Ok(_running) = self.run_lock.try_lock() else {
            return Err(op_failed(
                "another fix is running on this site; one run at a time",
            ));
        };
        if run.tenant != self.deps.site {
            return Err(HostError::WrongTenant);
        }
        let probe = self.probe().ok_or(HostError::NoWorld)?;
        let meta = self
            .deps
            .remedies
            .meta(&run.plan.remedy)
            .ok_or_else(|| op_failed("the remedy is not in the catalogue"))?;
        let plan = FixPlan::from_remedy(&meta, run.plan).map_err(|e| op_failed(e.code()))?;
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let fix = FixId::derive(plan.hash.as_str(), seq);
        let agent = AgentRef::new(
            &run.agent.id,
            run.agent.max_tier,
            run.agent.registered_deep(),
        );
        let ctx = FixContext::new(
            &self.deps.site,
            agent,
            DeploymentEpoch::new(self.deps.epoch),
            self.now(),
        );
        let job = Job {
            journal: Arc::clone(&self.deps.journal),
            policy: self.deps.policy.clone(),
            probe,
            targets: plan.affected.clone(),
            checks: Arc::clone(&self.deps.checks),
            applied: None,
            can_apply: true,
        };
        let mut side = self.side(
            Some(&run.direct),
            run.tenant,
            &run.agent.id,
            Some(&run.plan.remedy),
            &fix,
        );
        let (fix_run, plan_run, ctx_run) = (fix.clone(), plan.clone(), ctx.clone());
        let served = serve(&mut side, move |link| {
            job.execute(link, move |engine| {
                engine.run(&fix_run, &plan_run, &ctx_run)
            })
        })
        .await;
        let held_as = side.held_as.take();
        let (result, applied) = served.map_err(op_failed)?;
        let verified = matches!(&result, Ok(outcome) if outcome.state == FixState::Verified);
        self.settle_hold(held_as, verified).await;
        match result {
            Ok(outcome) if outcome.state == FixState::Verified => {
                let Some(applied) = applied else {
                    return Err(op_failed("the fix verified without an apply record"));
                };
                self.records
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        fix.to_string(),
                        Record {
                            remedy: run.plan.remedy.clone(),
                            agent_id: run.agent.id.clone(),
                            applied: applied.clone(),
                        },
                    );
                Ok(applied)
            }
            Ok(outcome) => Err(op_failed(format!(
                "the fix {fix} ended {}; the site is as it was",
                outcome.state.as_str()
            ))),
            Err(error) => Err(op_failed(error.code())),
        }
    }

    /// Releases the hold after a verified fix, or discards it after any other end.
    async fn settle_hold(&self, held_as: Option<String>, verified: bool) {
        let (Some(hold), Some(id)) = (&self.deps.hold, held_as) else {
            return;
        };
        let now = self.now();
        let outcome = if verified {
            hold.release(&id, now).await
        } else {
            hold.discard(&id, "the fix did not verify", now).await
        };
        if let Err(detail) = outcome {
            // The effects stay held (fail closed); say so where an operator looks.
            tracing::error!(
                site = %self.deps.site,
                hold = %id,
                "fix pipeline: the effect hold could not be settled; external effects stay held"
            );
            self.degrade(&format!(
                "the effect hold {id} could not be settled: {detail}"
            ));
        }
    }

    /// Undoes a verified fix inside its undo window: conflict-checked, journaled, audited and
    /// proved by the state reading back as it was.
    ///
    /// # Errors
    ///
    /// A [`FixError`]: not verified, window closed, a later change in the way, nothing to undo, or
    /// an infrastructure failure.
    pub async fn undo(&self, fix: &FixId, reason: &str) -> Result<FixOutcome, FixError> {
        let _running = self.run_lock.lock().await;
        let record = self
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(fix.as_str())
            .cloned()
            .ok_or_else(|| FixError::UndoUnavailable {
                state: "the process that applied the fix has ended".to_owned(),
            })?;
        let probe = self.probe().ok_or_else(|| FixError::Internal {
            detail: "no world is provided".to_owned(),
        })?;
        let ctx = FixContext::new(
            &self.deps.site,
            AgentRef::new(&record.agent_id, Tier::T3, false),
            DeploymentEpoch::new(self.deps.epoch),
            self.now(),
        );
        let job = Job {
            journal: Arc::clone(&self.deps.journal),
            policy: self.deps.policy.clone(),
            probe,
            targets: record
                .applied
                .changes
                .iter()
                .map(|c| c.target.clone())
                .collect(),
            checks: Arc::clone(&self.deps.checks),
            applied: Some(record.applied.clone()),
            can_apply: false,
        };
        let mut side = self.side(
            None,
            &self.deps.site,
            &record.agent_id,
            Some(&record.remedy),
            fix,
        );
        let (fix_run, reason) = (fix.clone(), reason.to_owned());
        let (result, _) = serve(&mut side, move |link| {
            job.execute(link, move |engine| engine.undo(&fix_run, &ctx, &reason))
        })
        .await
        .map_err(|reason| FixError::Internal {
            detail: reason.to_owned(),
        })?;
        let outcome = result?;
        if outcome.state == FixState::RolledBack {
            self.records
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(fix.as_str());
        }
        Ok(outcome)
    }

    /// The journaled outcome of a fix.
    ///
    /// # Errors
    ///
    /// [`FixError::WrongState`] for a fix with no records, [`FixError::JournalFailed`].
    pub async fn status(&self, fix: &FixId) -> Result<FixOutcome, FixError> {
        let journal = Arc::clone(&self.deps.journal);
        let fix_id = fix.clone();
        let mut side = self.side(None, &self.deps.site, "fix-pipeline", None, fix);
        let read = serve(&mut side, move |_link| {
            journal.entries(&fix_id).map(|entries| {
                if entries.is_empty() {
                    None
                } else {
                    Some(Progress::fold(&fix_id, entries).outcome())
                }
            })
        })
        .await
        .map_err(|reason| FixError::Internal {
            detail: reason.to_owned(),
        })?;
        read?.ok_or_else(|| FixError::WrongState {
            state: "absent".to_owned(),
            action: "read".to_owned(),
        })
    }

    /// One maintenance pass: recovers fixes the journal shows unfinished and closes the undo window
    /// of verified fixes whose time is up. A recovered fix that was applying cannot be reversed
    /// (the remedy undo log did not survive) and ends `Failed`.
    ///
    /// # Errors
    ///
    /// A [`FixError`] when the journal cannot be read.
    pub async fn maintain(&self) -> Result<Maintenance, FixError> {
        let _running = self.run_lock.lock().await;
        let Some(probe) = self.probe() else {
            return Ok(Maintenance::default());
        };
        let journal = Arc::clone(&self.deps.journal);
        let policy = self.deps.policy.clone();
        let checks = Arc::clone(&self.deps.checks);
        let now = self.now();
        let placeholder = FixId::derive("maintenance", 0);
        let mut side = self.side(None, &self.deps.site, "fix-pipeline", None, &placeholder);
        serve(&mut side, move |link| {
            let mut report = Maintenance::default();
            let entries = journal.all()?;
            let mut by_fix: std::collections::BTreeMap<
                FixId,
                Vec<spot_fix_pipeline::JournalEntry>,
            > = std::collections::BTreeMap::new();
            for entry in entries {
                by_fix.entry(entry.fix.clone()).or_default().push(entry);
            }
            for (fix, entries) in by_fix {
                let progress = Progress::fold(&fix, entries);
                let (Some(plan), Some(site), Some(agent), Some(epoch)) = (
                    progress.plan.clone(),
                    progress.site.clone(),
                    progress.agent.clone(),
                    progress.epoch,
                ) else {
                    continue;
                };
                let state = progress.state();
                let expired = state == FixState::Verified
                    && progress
                        .undo
                        .is_some_and(|w| !w.closed && w.is_expired(now));
                if state.is_terminal() && !expired {
                    continue;
                }
                let job = Job {
                    journal: Arc::clone(&journal),
                    policy: policy.clone(),
                    probe: probe.clone(),
                    targets: plan.affected.clone(),
                    checks: Arc::clone(&checks),
                    applied: None,
                    can_apply: false,
                };
                let ctx = FixContext::new(&site, agent, epoch, now);
                if expired {
                    let (closed, _) =
                        job.execute(link.clone(), |engine| engine.close_expired(&fix, now));
                    if matches!(closed, Ok(true)) {
                        report.closed += 1;
                    }
                } else {
                    let (outcome, _) =
                        job.execute(link.clone(), |engine| engine.recover(&fix, &ctx));
                    report.recovered += 1;
                    if !matches!(&outcome, Ok(o) if o.state != FixState::Failed) {
                        report.failed += 1;
                    }
                }
            }
            Ok::<Maintenance, FixError>(report)
        })
        .await
        .map_err(|reason| FixError::Internal {
            detail: reason.to_owned(),
        })?
    }
}

/// The [`ApplyPath`] the remedy host installs: every remedy above T1 goes through [`FixHost`].
pub struct PipelinePath(pub Arc<FixHost>);

impl ApplyPath for PipelinePath {
    fn apply<'a>(&'a self, run: PipelineRun<'a>) -> Fut<'a, Result<Applied, HostError>> {
        Box::pin(async move { self.0.apply_remedy(run).await })
    }
}
