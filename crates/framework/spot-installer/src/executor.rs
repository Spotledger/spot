//! The executor: it runs a ready [`InstallPlan`] step by step for one site, journalling every
//! step before and after it, and rolls the site back in reverse order when a step fails.
//!
//! It drives traits only ([`AppRegistryStore`], [`PatchRunner`], [`ComponentHost`],
//! [`FixtureLoader`], [`Snapshotter`]) and holds a per-site lease through `spot-coordinator`, so
//! two installers can never run on one site and a run that loses its lease stops at the next
//! journal write, leaving the journal for the next holder to [`Executor::resume`].
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use spot_installer::{Executor, Options};
//! # async fn run(ex: Executor, site: spot_core::SiteId, plan: spot_installer::InstallPlan) {
//! let options = Options::new("installer-1");
//! let outcome = ex.run(&site, &plan, plan.hash(), &options).await;
//! # let _ = outcome;
//! # }
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use spot_coordinator::{Coordinator, Lease};
use spot_core::{Clock, SiteId};

use crate::error::{
    INTERRUPTED, JOURNAL_CORRUPT, LEASE_HELD, LEASE_LOST, NEEDS_APPROVAL, NO_PLAN,
    PLAN_NOT_APPROVED, ROLLBACK_FAILED,
};
use crate::journal::{
    InstallJournal, LoadedComponent, SiteLifecycle, StepId, StepKind, StepState, Undo,
};
use crate::model::SiteState;
use crate::outcome::{ExecError, Failure, Outcome};
use crate::plan::{InstallPlan, Operation};
use crate::steps::{
    Step, Stop, done_steps, lease_name, loaded_from, merged_patches, push_fixtures, push_patches,
    require_installed, undone_so_far, upsert,
};
use crate::traits::{
    AppRegistryStore, ComponentHost, FixtureLoader, InstallChange, PatchRunner, Snapshotter,
    UpgradeChange,
};

/// The lease a run holds lasts this long by default; a crashed holder blocks nobody longer.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);

/// A plan that an administrator approved, with the hash the approval carries.
#[derive(Clone, Debug)]
pub struct ApprovedPlan {
    /// The plan to run.
    pub plan: InstallPlan,
    /// The hash the approval binds to; `run` refuses any other.
    pub hash: String,
}

/// Knobs of a run.
#[derive(Clone, Debug)]
pub struct Options {
    /// Who holds the site's lease; two runs on one site use different holders.
    pub holder: String,
    /// How long the lease lasts without renewal.
    pub lease_ttl: Duration,
}

impl Options {
    /// Options for a run held by `holder`, with the default lease ttl.
    #[must_use]
    pub fn new(holder: impl Into<String>) -> Self {
        Self {
            holder: holder.into(),
            lease_ttl: DEFAULT_LEASE_TTL,
        }
    }

    /// Sets the lease ttl.
    #[must_use]
    pub fn with_lease_ttl(mut self, ttl: Duration) -> Self {
        self.lease_ttl = ttl;
        self
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::new("installer")
    }
}

/// Runs install, upgrade and uninstall plans against one or many sites.
pub struct Executor {
    registry: Arc<dyn AppRegistryStore>,
    patches: Arc<dyn PatchRunner>,
    components: Arc<dyn ComponentHost>,
    fixtures: Arc<dyn FixtureLoader>,
    snapshots: Arc<dyn Snapshotter>,
    coordinator: Arc<dyn Coordinator>,
    clock: Arc<dyn Clock>,
}

/// The fixed context of one run: the site, the lease it holds and the plan hash it journals.
struct Ctx<'a> {
    site: &'a SiteId,
    lease: &'a Lease,
    plan_hash: &'a str,
}

impl Executor {
    /// An executor over its traits and the coordinator that holds the per-site lease.
    #[must_use]
    pub fn new(
        registry: Arc<dyn AppRegistryStore>,
        patches: Arc<dyn PatchRunner>,
        components: Arc<dyn ComponentHost>,
        fixtures: Arc<dyn FixtureLoader>,
        snapshots: Arc<dyn Snapshotter>,
        coordinator: Arc<dyn Coordinator>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            registry,
            patches,
            components,
            fixtures,
            snapshots,
            coordinator,
            clock,
        }
    }

    /// The current time, UTC seconds, from the injected clock.
    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// Runs `plan` on `site`, if `approval_hash` is the plan's hash and the plan is ready.
    ///
    /// On success every step ran ([`Outcome::Applied`]). On a step failure the site is put back
    /// in reverse order ([`Outcome::RolledBack`]); if a rollback step also fails, the run is
    /// [`Outcome::NeedsAttention`] and the site stays flagged `installing`. A refusal before
    /// anything changes — an unapproved plan, a plan waiting for capabilities, a lease another
    /// installer holds — is an [`ExecError`]; so is losing the lease mid-run, which leaves the
    /// journal for [`Executor::resume`].
    ///
    /// # Errors
    /// [`ExecError`] with `installer.plan_not_approved`, `installer.needs_approval`,
    /// `installer.lease_held`, `installer.lease_lost`, or a code a trait returned.
    pub async fn run(
        &self,
        site: &SiteId,
        plan: &InstallPlan,
        approval_hash: &str,
        options: &Options,
    ) -> Result<Outcome, ExecError> {
        if !plan.verify_hash(approval_hash) {
            return Err(ExecError::new(
                PLAN_NOT_APPROVED,
                format!(
                    "site {site}: the plan hash {} is not the approved hash {approval_hash}",
                    plan.hash()
                ),
            ));
        }
        if !plan.state().is_ready() {
            return Err(ExecError::new(
                NEEDS_APPROVAL,
                format!(
                    "site {site}: the plan waits for capability approval and never runs as it stands"
                ),
            ));
        }
        let lease = self.acquire(site, options).await?;
        // The compare-and-set comes first, before any read or step: a second node that got past
        // its own lease (a different coordinator, a lost lease) loses here, does no work and
        // leaves the site as it found it.
        if let Err(error) = self
            .move_lifecycle(site, SiteLifecycle::Idle, SiteLifecycle::Installing)
            .await
        {
            let _ = self.coordinator.release(&lease).await;
            return Err(error);
        }
        let steps = match self.plan_steps(site, plan).await {
            Ok(steps) => steps,
            Err(error) => {
                // Nothing ran: put the flag back, keep the original error.
                let _ = self
                    .move_lifecycle(site, SiteLifecycle::Installing, SiteLifecycle::Idle)
                    .await;
                let _ = self.coordinator.release(&lease).await;
                return Err(error);
            }
        };

        let ctx = Ctx {
            site,
            lease: &lease,
            plan_hash: plan.hash(),
        };
        let mut entries: Vec<InstallJournal> = Vec::new();
        let mut next = 0_u32;
        let stop = self.run_steps(&ctx, &steps, &mut entries, &mut next).await;
        match stop {
            Ok(()) => {
                self.finish(&ctx).await?;
                Ok(Outcome::Applied {
                    steps: done_steps(&entries),
                })
            }
            Err(Stop::LeaseLost(error)) => Err(error),
            Err(Stop::Failed(reason)) => {
                let failed_step = reason.step.clone();
                match self.rollback(&ctx, &mut entries).await {
                    Ok(undone) => {
                        self.finish(&ctx).await?;
                        Ok(Outcome::RolledBack {
                            failed_step,
                            reason,
                            undone,
                        })
                    }
                    Err(Stop::LeaseLost(error)) => Err(error),
                    Err(Stop::Failed(rollback_reason)) => {
                        // The site stays flagged `installing` and everything stays journalled.
                        Ok(Outcome::NeedsAttention {
                            failed_step: rollback_reason.step.clone(),
                            reason: rollback_reason,
                            undone: undone_so_far(&entries),
                        })
                    }
                }
            }
        }
    }

    /// Finishes a run that stopped before its commit, or verifies and completes one that stopped
    /// after it, so a site is always in its old state or its new one, never between.
    ///
    /// Reads the site's journal; a run interrupted before its commit is rolled back, one
    /// interrupted after it is smoke-tested and completed (rolled back if the check fails). It is
    /// idempotent: a second `resume` after the first finds no journal and returns
    /// [`Outcome::Nothing`].
    ///
    /// # Errors
    /// [`ExecError`] with `installer.lease_held`, `installer.lease_lost` or
    /// `installer.journal_corrupt`.
    pub async fn resume(&self, site: &SiteId) -> Result<Outcome, ExecError> {
        let options = Options::new("installer-resume");
        let lease = self.acquire(site, &options).await?;
        let rows = match self.registry.journal(site).await {
            Ok(rows) => rows,
            Err(error) => {
                let _ = self.coordinator.release(&lease).await;
                return Err(error);
            }
        };
        if rows.is_empty() {
            // Nothing was interrupted; make sure a run that stopped before its first journal
            // write does not leave the site flagged installing.
            let cleared = self.clear_stale_flag(site).await;
            let _ = self.coordinator.release(&lease).await;
            cleared?;
            return Ok(Outcome::Nothing);
        }
        let plan_hash = rows[0].plan_hash.clone();
        if rows.iter().any(|row| row.plan_hash != plan_hash) {
            return Err(ExecError::new(
                JOURNAL_CORRUPT,
                format!("site {site}: the journal mixes two runs and cannot be trusted"),
            ));
        }
        let mut entries = rows;
        let ctx = Ctx {
            site,
            lease: &lease,
            plan_hash: &plan_hash,
        };
        let committed: Vec<(String, Option<LoadedComponent>)> = entries
            .iter()
            .filter(|row| row.step.kind == StepKind::Commit && row.state == StepState::Done)
            .filter_map(|row| match &row.undo {
                Some(Undo::Registry { app, after, .. }) => Some((app.clone(), after.clone())),
                _ => None,
            })
            .collect();
        if !committed.is_empty() {
            match self.complete_committed(site, &lease, &committed).await {
                Ok(()) => {
                    self.finish(&ctx).await?;
                    return Ok(Outcome::Applied {
                        steps: done_steps(&entries),
                    });
                }
                Err(error) if error.code == LEASE_LOST => return Err(error),
                Err(_) => {}
            }
        }
        // A pure compare: `Installing -> Installing` writes nothing new, it only proves the flag
        // is what a journal implies. A journal with an `Idle` flag cannot be produced by
        // `finish` (it clears the journal before it sets the flag), so it is refused fail
        // closed here, and the lease is given back so the next holder is not locked out.
        if let Err(error) = self
            .move_lifecycle(site, SiteLifecycle::Installing, SiteLifecycle::Installing)
            .await
        {
            let _ = self.coordinator.release(&lease).await;
            return Err(error);
        }
        let stopped = entries
            .iter()
            .max_by_key(|row| row.step.index)
            .map(|row| row.step.clone())
            .unwrap_or_else(|| StepId::new(0, StepKind::Commit, ""));
        match self.rollback(&ctx, &mut entries).await {
            Ok(undone) => {
                self.finish(&ctx).await?;
                Ok(Outcome::RolledBack {
                    failed_step: stopped.clone(),
                    reason: Failure::new(
                        stopped,
                        INTERRUPTED,
                        format!("site {site}: the run was interrupted and the site was put back"),
                    ),
                    undone,
                })
            }
            Err(Stop::LeaseLost(error)) => Err(error),
            Err(Stop::Failed(reason)) => Ok(Outcome::NeedsAttention {
                failed_step: reason.step.clone(),
                reason,
                undone: undone_so_far(&entries),
            }),
        }
    }

    /// Runs a plan on each of `sites`, bounded by `concurrency`, and returns one result per site.
    ///
    /// Sites are independent: a failure on one never stops, changes or rolls back another, and
    /// each result is that site's own [`Outcome`]. A site with no entry in `plans` gets an
    /// `installer.no_plan` error and does not affect the others.
    pub async fn run_many(
        &self,
        sites: &[SiteId],
        plans: &BTreeMap<SiteId, ApprovedPlan>,
        options: &Options,
        concurrency: usize,
    ) -> Vec<(SiteId, Result<Outcome, ExecError>)> {
        use futures::stream::{self, StreamExt};
        let bounded = concurrency.max(1);
        stream::iter(sites.iter().cloned())
            .map(|site| {
                let approved = plans.get(&site);
                async move {
                    let result = match approved {
                        Some(approved) => {
                            self.run(&site, &approved.plan, &approved.hash, options)
                                .await
                        }
                        None => Err(ExecError::new(
                            NO_PLAN,
                            format!("site {site}: no approved plan was given for it"),
                        )),
                    };
                    (site, result)
                }
            })
            .buffer_unordered(bounded)
            .collect()
            .await
    }

    /// Takes the site's lease, or refuses with the coordinator's reason mapped to `installer.*`.
    async fn acquire(&self, site: &SiteId, options: &Options) -> Result<Lease, ExecError> {
        let name = lease_name(site);
        self.coordinator
            .acquire(&name, &options.holder, options.lease_ttl)
            .await
            .map_err(|error| {
                let code = if error.code == spot_coordinator::error::LEASE_HELD {
                    LEASE_HELD
                } else {
                    LEASE_LOST
                };
                ExecError::new(code, format!("site {site}: {}", error.message))
            })
    }

    /// Moves the site's lifecycle flag from the state the caller read to the next one, through
    /// the registry's conditional move. A conflict becomes `installer.lease_held` (a second
    /// installer) or `installer.step_failed` (any other stale view); nothing is overwritten.
    async fn move_lifecycle(
        &self,
        site: &SiteId,
        from: SiteLifecycle,
        to: SiteLifecycle,
    ) -> Result<(), ExecError> {
        self.registry
            .set_lifecycle(site, from, to)
            .await
            .map_err(|error| error.into_exec_error(site))
    }

    /// Puts a site back to idle if a run stopped before its first journal write left it flagged
    /// installing; an idle site is left alone.
    async fn clear_stale_flag(&self, site: &SiteId) -> Result<(), ExecError> {
        if self.registry.lifecycle(site).await? == SiteLifecycle::Installing {
            self.move_lifecycle(site, SiteLifecycle::Installing, SiteLifecycle::Idle)
                .await?;
        }
        Ok(())
    }

    /// Reads the site and builds the steps for `plan`.
    async fn plan_steps(&self, site: &SiteId, plan: &InstallPlan) -> Result<Vec<Step>, ExecError> {
        let before = self.registry.load(site).await?;
        self.build_steps(site, plan, &before).await
    }

    /// Checks the site's fence: the lease is still ours.
    async fn fence(&self, site: &SiteId, lease: &Lease) -> Result<(), ExecError> {
        let name = lease_name(site);
        self.coordinator
            .check_fence(&name, lease.token)
            .await
            .map_err(|error| {
                ExecError::new(
                    LEASE_LOST,
                    format!("site {site}: the lease was lost ({})", error.message),
                )
            })
    }

    /// Clears the journal, marks the site idle and gives the lease back: the run is finished.
    async fn finish(&self, ctx: &Ctx<'_>) -> Result<(), ExecError> {
        self.fence(ctx.site, ctx.lease).await?;
        self.registry.clear_journal(ctx.site).await?;
        self.move_lifecycle(ctx.site, SiteLifecycle::Installing, SiteLifecycle::Idle)
            .await?;
        // Best effort: a lease that was lost is already gone, and the ttl releases the rest.
        let _ = self.coordinator.release(ctx.lease).await;
        Ok(())
    }

    /// Finishes a committed run: loads each committed component (idempotently) and smoke-tests
    /// it, so a run interrupted between the commit and the load completes, never half-loaded.
    async fn complete_committed(
        &self,
        site: &SiteId,
        lease: &Lease,
        committed: &[(String, Option<LoadedComponent>)],
    ) -> Result<(), ExecError> {
        for (app, after) in committed {
            self.fence(site, lease).await?;
            match after {
                Some(loaded) => {
                    self.components
                        .load(site, app, &loaded.version, loaded.digest)
                        .await?;
                    self.components.smoke_test(site, app).await?;
                }
                None => self.components.unload(site, app).await?,
            }
        }
        Ok(())
    }

    /// Writes one journal row, after checking the fence; the effect runs between two calls.
    async fn record(
        &self,
        ctx: &Ctx<'_>,
        step: &StepId,
        state: StepState,
        undo: Option<Undo>,
        entries: &mut Vec<InstallJournal>,
    ) -> Result<(), Stop> {
        self.fence(ctx.site, ctx.lease)
            .await
            .map_err(Stop::LeaseLost)?;
        let row = InstallJournal::new(ctx.plan_hash, step.clone(), state, self.now(), undo);
        self.registry
            .record(ctx.site, row.clone())
            .await
            .map_err(|error| Stop::Failed(Failure::new(step.clone(), error.code, error.message)))?;
        upsert(entries, row);
        Ok(())
    }

    /// Builds the ordered steps of a plan from the site state read before the run.
    async fn build_steps(
        &self,
        site: &SiteId,
        plan: &InstallPlan,
        before: &SiteState,
    ) -> Result<Vec<Step>, ExecError> {
        let mut steps: Vec<Step> = Vec::new();
        let mut snapshot = plan.purges();
        for action in plan.actions() {
            if !action.reversible {
                snapshot = true;
            }
            match &action.op {
                Operation::Install {
                    app,
                    version,
                    digest,
                } => {
                    push_patches(&mut steps, app, action);
                    push_fixtures(&mut steps, app, action);
                    steps.push(Step::CommitInstall {
                        app: app.clone(),
                        change: InstallChange {
                            version: version.clone(),
                            digest: *digest,
                            approved: action.capabilities.clone(),
                            applied_patches: action
                                .patches
                                .iter()
                                .map(|patch| patch.id.clone())
                                .collect(),
                        },
                        before: before.get(app).cloned(),
                    });
                    steps.push(Step::Load {
                        app: app.clone(),
                        version: version.clone(),
                        digest: *digest,
                        before: loaded_from(before.get(app)),
                    });
                    steps.push(Step::Smoke { app: app.clone() });
                }
                Operation::Upgrade {
                    app,
                    from,
                    to,
                    digest,
                } => {
                    push_patches(&mut steps, app, action);
                    push_fixtures(&mut steps, app, action);
                    let current = require_installed(before, app)?;
                    steps.push(Step::CommitUpgrade {
                        app: app.clone(),
                        change: UpgradeChange {
                            from: from.clone(),
                            to: to.clone(),
                            digest: *digest,
                            approved: action.capabilities.clone(),
                            applied_patches: merged_patches(&current.applied_patches, action),
                        },
                        before: current.clone(),
                    });
                    steps.push(Step::Load {
                        app: app.clone(),
                        version: to.clone(),
                        digest: *digest,
                        before: loaded_from(before.get(app)),
                    });
                    steps.push(Step::Smoke { app: app.clone() });
                }
                Operation::Uninstall { app, purge } => {
                    let owned = self.fixtures.owned(site, app).await?;
                    for fixture in owned {
                        steps.push(Step::RemoveFixture {
                            app: app.clone(),
                            fixture,
                        });
                    }
                    let doctypes = self.registry.owned_doctypes(site, app).await?;
                    let current = require_installed(before, app)?;
                    steps.push(Step::CommitUninstall {
                        app: app.clone(),
                        purge: *purge,
                        before: current.clone(),
                        doctypes,
                    });
                    steps.push(Step::Unload {
                        app: app.clone(),
                        before: loaded_from(before.get(app)).unwrap_or_else(|| {
                            LoadedComponent::new(current.version.clone(), current.digest)
                        }),
                    });
                }
            }
        }
        if snapshot {
            steps.insert(0, Step::Snapshot);
        }
        Ok(steps)
    }

    /// Runs the steps in order, journalling each before and after it.
    async fn run_steps(
        &self,
        ctx: &Ctx<'_>,
        steps: &[Step],
        entries: &mut Vec<InstallJournal>,
        next: &mut u32,
    ) -> Result<(), Stop> {
        for step in steps {
            let index = *next;
            *next += 1;
            self.run_one(ctx, index, step, entries).await?;
        }
        Ok(())
    }

    /// Runs one step.
    async fn run_one(
        &self,
        ctx: &Ctx<'_>,
        index: u32,
        step: &Step,
        entries: &mut Vec<InstallJournal>,
    ) -> Result<(), Stop> {
        match step {
            Step::Snapshot => {
                let id = StepId::new(index, StepKind::Snapshot, "");
                self.record(ctx, &id, StepState::Started, None, entries)
                    .await?;
                let snapshot = self.snapshots.take(ctx.site).await.map_err(|error| {
                    Stop::Failed(Failure::new(id.clone(), error.code, error.message))
                })?;
                self.record(
                    ctx,
                    &id,
                    StepState::Done,
                    Some(Undo::Snapshot(snapshot)),
                    entries,
                )
                .await
            }
            Step::Patch {
                app,
                patch_id,
                reversible,
            } => {
                let id = StepId::new(index, StepKind::Patch, patch_id.clone());
                let undo = Undo::Patch {
                    app: app.clone(),
                    patch_id: patch_id.clone(),
                    reversible: *reversible,
                };
                self.record(ctx, &id, StepState::Started, Some(undo.clone()), entries)
                    .await?;
                let outcome =
                    self.patches
                        .apply(ctx.site, app, patch_id)
                        .await
                        .map_err(|error| {
                            Stop::Failed(Failure::new(id.clone(), error.code, error.message))
                        })?;
                let undo = outcome.is_applied().then_some(undo);
                self.record(ctx, &id, StepState::Done, undo, entries).await
            }
            Step::Fixture { app, fixture } => {
                let id = StepId::new(index, StepKind::Fixture, fixture.clone());
                let undo = Undo::FixtureLoaded {
                    app: app.clone(),
                    fixture: fixture.clone(),
                };
                self.record(ctx, &id, StepState::Started, Some(undo.clone()), entries)
                    .await?;
                self.fixtures
                    .load(ctx.site, app, fixture)
                    .await
                    .map_err(|error| {
                        Stop::Failed(Failure::new(id.clone(), error.code, error.message))
                    })?;
                self.record(ctx, &id, StepState::Done, Some(undo), entries)
                    .await
            }
            Step::RemoveFixture { app, fixture } => {
                let id = StepId::new(index, StepKind::RemoveFixture, fixture.clone());
                let undo = Undo::FixtureRemoved {
                    app: app.clone(),
                    fixture: fixture.clone(),
                };
                self.record(ctx, &id, StepState::Started, Some(undo.clone()), entries)
                    .await?;
                self.fixtures
                    .remove(ctx.site, app, fixture)
                    .await
                    .map_err(|error| {
                        Stop::Failed(Failure::new(id.clone(), error.code, error.message))
                    })?;
                self.record(ctx, &id, StepState::Done, Some(undo), entries)
                    .await
            }
            Step::CommitInstall {
                app,
                change,
                before,
            } => {
                let id = StepId::new(index, StepKind::Commit, app.clone());
                let undo = Undo::Registry {
                    app: app.clone(),
                    before: before.clone().map(Box::new),
                    after: Some(LoadedComponent::new(change.version.clone(), change.digest)),
                    doctypes: Vec::new(),
                };
                self.record(ctx, &id, StepState::Started, Some(undo.clone()), entries)
                    .await?;
                self.fence(ctx.site, ctx.lease)
                    .await
                    .map_err(Stop::LeaseLost)?;
                let row = InstallJournal::new(
                    ctx.plan_hash,
                    id.clone(),
                    StepState::Done,
                    self.now(),
                    Some(undo),
                );
                self.registry
                    .commit_install(ctx.site, app, change.clone(), row.clone())
                    .await
                    .map_err(|error| {
                        Stop::Failed(Failure::new(id.clone(), error.code, error.message))
                    })?;
                upsert(entries, row);
                Ok(())
            }
            Step::CommitUpgrade {
                app,
                change,
                before,
            } => {
                let id = StepId::new(index, StepKind::Commit, app.clone());
                let undo = Undo::Registry {
                    app: app.clone(),
                    before: Some(Box::new(before.clone())),
                    after: Some(LoadedComponent::new(change.to.clone(), change.digest)),
                    doctypes: Vec::new(),
                };
                self.record(ctx, &id, StepState::Started, Some(undo.clone()), entries)
                    .await?;
                self.fence(ctx.site, ctx.lease)
                    .await
                    .map_err(Stop::LeaseLost)?;
                let row = InstallJournal::new(
                    ctx.plan_hash,
                    id.clone(),
                    StepState::Done,
                    self.now(),
                    Some(undo),
                );
                self.registry
                    .commit_upgrade(ctx.site, app, change.clone(), row.clone())
                    .await
                    .map_err(|error| {
                        Stop::Failed(Failure::new(id.clone(), error.code, error.message))
                    })?;
                upsert(entries, row);
                Ok(())
            }
            Step::CommitUninstall {
                app,
                purge,
                before,
                doctypes,
            } => {
                let id = StepId::new(index, StepKind::Commit, app.clone());
                let undo = Undo::Registry {
                    app: app.clone(),
                    before: Some(Box::new(before.clone())),
                    after: None,
                    doctypes: doctypes.clone(),
                };
                self.record(ctx, &id, StepState::Started, Some(undo.clone()), entries)
                    .await?;
                self.fence(ctx.site, ctx.lease)
                    .await
                    .map_err(Stop::LeaseLost)?;
                let row = InstallJournal::new(
                    ctx.plan_hash,
                    id.clone(),
                    StepState::Done,
                    self.now(),
                    Some(undo),
                );
                self.registry
                    .commit_uninstall(ctx.site, app, *purge, row.clone())
                    .await
                    .map_err(|error| {
                        Stop::Failed(Failure::new(id.clone(), error.code, error.message))
                    })?;
                upsert(entries, row);
                Ok(())
            }
            Step::Load {
                app,
                version,
                digest,
                before,
            } => {
                let id = StepId::new(index, StepKind::Load, app.clone());
                let undo = Undo::Component {
                    app: app.clone(),
                    before: before.clone(),
                };
                self.record(ctx, &id, StepState::Started, Some(undo.clone()), entries)
                    .await?;
                self.components
                    .load(ctx.site, app, version, *digest)
                    .await
                    .map_err(|error| {
                        Stop::Failed(Failure::new(id.clone(), error.code, error.message))
                    })?;
                self.record(ctx, &id, StepState::Done, Some(undo), entries)
                    .await
            }
            Step::Unload { app, before } => {
                let id = StepId::new(index, StepKind::Unload, app.clone());
                let undo = Undo::Component {
                    app: app.clone(),
                    before: Some(before.clone()),
                };
                self.record(ctx, &id, StepState::Started, Some(undo.clone()), entries)
                    .await?;
                self.components
                    .unload(ctx.site, app)
                    .await
                    .map_err(|error| {
                        Stop::Failed(Failure::new(id.clone(), error.code, error.message))
                    })?;
                self.record(ctx, &id, StepState::Done, Some(undo), entries)
                    .await
            }
            Step::Smoke { app } => {
                let id = StepId::new(index, StepKind::Smoke, app.clone());
                self.record(ctx, &id, StepState::Started, None, entries)
                    .await?;
                self.components
                    .smoke_test(ctx.site, app)
                    .await
                    .map_err(|error| {
                        Stop::Failed(Failure::new(id.clone(), error.code, error.message))
                    })?;
                self.record(ctx, &id, StepState::Done, None, entries).await
            }
        }
    }

    /// Rolls the site back in reverse step order, journalling each step undone.
    async fn rollback(
        &self,
        ctx: &Ctx<'_>,
        entries: &mut Vec<InstallJournal>,
    ) -> Result<Vec<StepId>, Stop> {
        let mut undone: Vec<StepId> = Vec::new();
        let order: Vec<usize> = (0..entries.len()).rev().collect();
        for index in order {
            let entry = entries[index].clone();
            if entry.state == StepState::Undone {
                continue;
            }
            let Some(undo) = entry.undo.clone() else {
                continue;
            };
            // Mark the step as being undone before the effect, so a store (and a fake) learns
            // which step is rolling back, and the fence is checked for this write too.
            self.record(ctx, &entry.step, entry.state, Some(undo.clone()), entries)
                .await?;
            self.undo_one(ctx, &entry.step, &undo)
                .await
                .map_err(|error| {
                    Stop::Failed(Failure::new(
                        entry.step.clone(),
                        ROLLBACK_FAILED,
                        error.message,
                    ))
                })?;
            self.record(ctx, &entry.step, StepState::Undone, Some(undo), entries)
                .await?;
            undone.push(entry.step);
        }
        Ok(undone)
    }

    /// Undoes one step's effect.
    async fn undo_one(&self, ctx: &Ctx<'_>, step: &StepId, undo: &Undo) -> Result<(), ExecError> {
        match undo {
            Undo::Snapshot(id) => self.snapshots.restore(ctx.site, id).await,
            Undo::Patch {
                app,
                patch_id,
                reversible,
            } => {
                if *reversible {
                    self.patches.undo(ctx.site, app, patch_id).await
                } else {
                    Ok(())
                }
            }
            Undo::FixtureLoaded { app, fixture } => {
                self.fixtures.remove(ctx.site, app, fixture).await
            }
            Undo::FixtureRemoved { app, fixture } => {
                self.fixtures.load(ctx.site, app, fixture).await
            }
            Undo::Registry {
                app,
                before,
                after: _,
                doctypes: _,
            } => {
                let row = InstallJournal::new(
                    ctx.plan_hash,
                    step.clone(),
                    StepState::Undone,
                    self.now(),
                    Some(undo.clone()),
                );
                match before {
                    Some(app_state) => self.registry.restore(ctx.site, app_state, row).await,
                    None => {
                        self.registry
                            .commit_uninstall(ctx.site, app, false, row)
                            .await
                    }
                }
            }
            Undo::Component { app, before } => match before {
                Some(loaded) => {
                    self.components
                        .load(ctx.site, app, &loaded.version, loaded.digest)
                        .await
                }
                None => self.components.unload(ctx.site, app).await,
            },
        }
    }
}
