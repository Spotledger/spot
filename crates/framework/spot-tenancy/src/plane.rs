//! The control plane: provisioning as a journaled, resumable workflow with compensations, and
//! deprovisioning as suspend, verified archive, retention delay and two-person deletion.
//!
//! Fail-closed rules enforced here:
//! - a step is journaled `Started` before it runs, so a crash mid-step is compensated on rollback;
//! - nothing is hard-deleted before the retention delay, a verified backup (re-verified at
//!   deletion time), no legal hold, and two distinct approvers who are not the requester;
//! - an approval hook that returns nothing is a refusal;
//! - the audit trail records every attempt including failures, and deletion records its intent
//!   before it destroys anything (a failed audit write aborts it);
//! - the residency region is never relaxed to find capacity.

use std::collections::BTreeSet;

use crate::error::{Error, Result, codes};
use crate::model::{
    Edition, Placement, Region, Retention, Tenant, TenantFlags, TenantId, TenantName, TenantState,
};
use crate::placement::{Cluster, PlacementMap};
use crate::ports::{ApprovalHook, Archiver, AuditEvent, AuditSink, Outcome, Ports, StepFailure};
use crate::store::{ControlStore, JournalEntry, JournalKind, Step};

/// Estimated cost of each step in milliseconds, used by the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepCosts {
    /// Cost of [`Step::CreateSchema`].
    pub create_schema: u64,
    /// Cost of [`Step::Seed`].
    pub seed: u64,
    /// Cost of [`Step::CreateUsers`].
    pub create_users: u64,
    /// Cost of [`Step::IssueLicence`].
    pub issue_licence: u64,
    /// Cost of [`Step::RegisterTelemetry`].
    pub register_telemetry: u64,
    /// Cost of [`Step::Activate`].
    pub activate: u64,
}

impl Default for StepCosts {
    fn default() -> Self {
        Self {
            create_schema: 20_000,
            seed: 90_000,
            create_users: 10_000,
            issue_licence: 5_000,
            register_telemetry: 5_000,
            activate: 1_000,
        }
    }
}

impl StepCosts {
    /// The cost of one step.
    #[must_use]
    pub fn of(&self, step: Step) -> u64 {
        match step {
            Step::CreateSchema => self.create_schema,
            Step::Seed => self.seed,
            Step::CreateUsers => self.create_users,
            Step::IssueLicence => self.issue_licence,
            Step::RegisterTelemetry => self.register_telemetry,
            Step::Activate => self.activate,
        }
    }
}

/// Policy knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Step cost model.
    pub costs: StepCosts,
    /// The shortest retention delay an archive may ask for (default seven days).
    pub min_retention_ms: u64,
    /// How long a deleted tenant's name stays reserved (default ninety days).
    pub name_reservation_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            costs: StepCosts::default(),
            min_retention_ms: 7 * 86_400_000,
            name_reservation_ms: 90 * 86_400_000,
        }
    }
}

/// A request for a new tenant. Time and entropy are arguments: the control plane reads no clock
/// and no random source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionRequest {
    /// The requested name.
    pub name: String,
    /// The residency region.
    pub region: String,
    /// The licence edition.
    pub edition: String,
    /// Entropy for the UUIDv7 (ten bytes from a CSPRNG owned by the caller).
    pub entropy: [u8; 10],
    /// Initial flags.
    pub flags: TenantFlags,
}

/// What `provision` decided: the tenant, where it goes and the steps with their estimates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionPlan {
    /// The new tenant's id.
    pub tenant: TenantId,
    /// Where it will live.
    pub placement: Placement,
    /// The steps in order with their estimated cost in milliseconds.
    pub steps: Vec<(Step, u64)>,
    /// The sum of the estimates.
    pub estimated_ms: u64,
}

/// The control plane over a store and the ports.
pub struct ControlPlane<'a> {
    /// The registry.
    pub store: &'a dyn ControlStore,
    /// The provisioning ports.
    pub ports: Ports<'a>,
    /// The final-backup port.
    pub archiver: &'a dyn Archiver,
    /// The two-person approval hook.
    pub approvals: &'a dyn ApprovalHook,
    /// The audit trail.
    pub audit: &'a dyn AuditSink,
    /// Databases that can hold tenants.
    pub clusters: &'a [Cluster],
    /// Policy.
    pub config: Config,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Untouched,
    Started,
    Done,
    Compensated,
}

fn status(journal: &[JournalEntry], step: Step) -> Status {
    match journal
        .iter()
        .rev()
        .find(|e| e.step == step)
        .map(|e| e.kind)
    {
        None => Status::Untouched,
        Some(JournalKind::Started) => Status::Started,
        Some(JournalKind::Done) => Status::Done,
        Some(JournalKind::Compensated) => Status::Compensated,
    }
}

fn step_failed(tenant: &TenantId, step: Step, f: StepFailure) -> Error {
    Error::StepFailed {
        tenant: tenant.to_string(),
        step: step.name(),
        reason: f.reason,
    }
}

fn normalise(principal: &str) -> String {
    principal.trim().to_lowercase()
}

impl ControlPlane<'_> {
    fn check_actor(actor: &str) -> Result<()> {
        if actor.trim().is_empty() {
            Err(Error::InvalidRequest {
                reason: "an operation needs a named actor",
            })
        } else {
            Ok(())
        }
    }

    fn record(
        &self,
        id: &TenantId,
        action: &'static str,
        step: Option<&'static str>,
        actor: &str,
        outcome: Outcome,
        at: u64,
    ) -> Result<()> {
        let event = AuditEvent {
            tenant: *id,
            action,
            step,
            actor: actor.to_owned(),
            outcome,
            at,
        };
        self.audit
            .record(&event)
            .map_err(|_| Error::AuditFailed { action })
    }

    /// Runs `f`, then audits the outcome. A failure is audited best-effort and the original error
    /// is returned; a success whose audit write fails is reported as [`Error::AuditFailed`].
    fn audited<T>(
        &self,
        id: &TenantId,
        action: &'static str,
        actor: &str,
        now: u64,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        match Self::check_actor(actor).and_then(|()| f()) {
            Ok(v) => {
                self.record(id, action, None, actor, Outcome::Success, now)?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.record(id, action, None, actor, Outcome::Failure(e.code()), now);
                Err(e)
            }
        }
    }

    fn journal(&self, id: &TenantId, step: Step, kind: JournalKind, at: u64) -> Result<()> {
        self.store
            .journal_append(id, JournalEntry { step, kind, at })
    }

    // ---- provisioning -------------------------------------------------------------------

    /// Validates the request, chooses a placement and registers the tenant in `Provisioning`.
    /// Nothing is created on the ports yet; call [`run`](Self::run) next.
    ///
    /// # Errors
    /// Invalid request or name, name taken or reserved, no placement, audit or store failure.
    pub fn provision(
        &self,
        req: &ProvisionRequest,
        actor: &str,
        now: u64,
    ) -> Result<ProvisionPlan> {
        let id = TenantId::new_v7(now, req.entropy);
        self.audited(&id, "provision_plan", actor, now, || {
            let name = TenantName::new(req.name.clone())?;
            let region = Region::new(req.region.clone())?;
            let edition = Edition::new(req.edition.clone())?;
            let placement = PlacementMap::choose(&id, &region, self.clusters, &self.store.load()?)?;
            // The store enforces the chosen database's bound atomically with the insert; a
            // placement the map did not offer it cannot name, and then nothing is admitted.
            let capacity = self
                .clusters
                .iter()
                .find(|c| c.cluster == placement.cluster && c.database == placement.database)
                .map_or(0, |c| c.capacity);
            let tenant = Tenant {
                id,
                name,
                region,
                edition,
                state: TenantState::Provisioning,
                placement: placement.clone(),
                created: now,
                flags: req.flags,
                version: 0,
                retention: None,
                name_reserved_until: None,
            };
            self.store.insert_within(tenant, now, capacity)?;
            let steps: Vec<(Step, u64)> = Step::ALL
                .iter()
                .map(|s| (*s, self.config.costs.of(*s)))
                .collect();
            let estimated_ms = steps
                .iter()
                .fold(0u64, |acc, (_, c)| acc.saturating_add(*c));
            Ok(ProvisionPlan {
                tenant: id,
                placement,
                steps,
                estimated_ms,
            })
        })
    }

    fn apply(&self, step: Step, tenant: &Tenant) -> std::result::Result<(), StepFailure> {
        match step {
            Step::CreateSchema => self.ports.schema.create(tenant),
            Step::Seed => self.ports.seeder.seed(tenant),
            Step::CreateUsers => self.ports.iam.create_users(tenant),
            Step::IssueLicence => self.ports.licence.issue(tenant),
            Step::RegisterTelemetry => self.ports.telemetry.register(tenant),
            Step::Activate => Ok(()),
        }
    }

    fn undo(&self, step: Step, tenant: &Tenant) -> std::result::Result<(), StepFailure> {
        match step {
            Step::CreateSchema => self.ports.schema.drop_schema(tenant),
            Step::Seed => self.ports.seeder.unseed(tenant),
            Step::CreateUsers => self.ports.iam.remove_users(tenant),
            Step::IssueLicence => self.ports.licence.revoke(tenant),
            Step::RegisterTelemetry => self.ports.telemetry.deregister(tenant),
            Step::Activate => Ok(()),
        }
    }

    /// Runs (or resumes) the provisioning steps. Steps already `Done` in the journal are skipped;
    /// a step left `Started` by a crash is simply run again (every port call is idempotent).
    ///
    /// # Errors
    /// [`Error::StepFailed`] naming the step (the journal keeps the position: call `run` again to
    /// resume or [`rollback`](Self::rollback) to clean up); wrong state; journal or store failure.
    pub fn run(&self, id: &TenantId, actor: &str, now: u64) -> Result<Tenant> {
        self.audited(id, "provision_run", actor, now, || {
            self.run_inner(id, actor, now)
        })
    }

    fn run_inner(&self, id: &TenantId, actor: &str, now: u64) -> Result<Tenant> {
        let mut tenant = self.store.get(id)?;
        let journal = self.store.journal(id)?;
        if journal.iter().any(|e| e.kind == JournalKind::Compensated) {
            return Err(Error::WrongState {
                tenant: id.to_string(),
                state: tenant.state,
                operation: "resume a provisioning that is being rolled back",
            });
        }
        let finishing_activation = tenant.state == TenantState::Active
            && status(&journal, Step::Activate) == Status::Started;
        if tenant.state != TenantState::Provisioning && !finishing_activation {
            return Err(Error::WrongState {
                tenant: id.to_string(),
                state: tenant.state,
                operation: "run provisioning",
            });
        }
        // Steps complete strictly in order: nothing may be touched after the first incomplete one.
        let mut seen_incomplete = false;
        for step in Step::ALL {
            match (status(&journal, step), seen_incomplete) {
                (Status::Done, false) => {}
                (Status::Done | Status::Started, true) => {
                    return Err(Error::JournalCorrupt {
                        tenant: id.to_string(),
                        reason: "a step ran before its predecessor finished",
                    });
                }
                (Status::Started | Status::Untouched, _) => seen_incomplete = true,
                (Status::Compensated, _) => {
                    return Err(Error::JournalCorrupt {
                        tenant: id.to_string(),
                        reason: "unexpected compensation",
                    });
                }
            }
        }
        for step in Step::ALL {
            if status(&journal, step) == Status::Done {
                continue;
            }
            self.journal(id, step, JournalKind::Started, now)?;
            let result = if step == Step::Activate {
                if tenant.state == TenantState::Provisioning {
                    let mut next = tenant.clone();
                    next.state = TenantState::Active;
                    tenant = self.store.update(next, tenant.version)?;
                }
                Ok(())
            } else {
                self.apply(step, &tenant)
            };
            match result {
                Ok(()) => {
                    self.journal(id, step, JournalKind::Done, now)?;
                    self.record(
                        id,
                        "provision_step",
                        Some(step.name()),
                        actor,
                        Outcome::Success,
                        now,
                    )?;
                }
                Err(f) => {
                    let _ = self.record(
                        id,
                        "provision_step",
                        Some(step.name()),
                        actor,
                        Outcome::Failure(codes::STEP_FAILED),
                        now,
                    );
                    return Err(step_failed(id, step, f));
                }
            }
        }
        Ok(tenant)
    }

    /// Compensates a failed or abandoned provisioning in reverse order and marks the tenant
    /// `Deleted` with its name released at once (no data ever existed under it). Safe to repeat
    /// after a failure: steps already compensated are skipped.
    ///
    /// # Errors
    /// [`Error::WrongState`] unless the tenant is still `Provisioning` (an activated tenant goes
    /// through deprovisioning, never through rollback); [`Error::StepFailed`] when a compensation fails.
    pub fn rollback(&self, id: &TenantId, actor: &str, now: u64) -> Result<Tenant> {
        self.audited(id, "provision_rollback", actor, now, || {
            let tenant = self.store.get(id)?;
            if tenant.state != TenantState::Provisioning {
                return Err(Error::WrongState {
                    tenant: id.to_string(),
                    state: tenant.state,
                    operation: "roll back provisioning",
                });
            }
            let journal = self.store.journal(id)?;
            for step in Step::ALL.iter().rev().copied() {
                if matches!(status(&journal, step), Status::Started | Status::Done) {
                    if let Err(f) = self.undo(step, &tenant) {
                        let _ = self.record(
                            id,
                            "rollback_step",
                            Some(step.name()),
                            actor,
                            Outcome::Failure(codes::STEP_FAILED),
                            now,
                        );
                        return Err(step_failed(id, step, f));
                    }
                    self.journal(id, step, JournalKind::Compensated, now)?;
                    self.record(
                        id,
                        "rollback_step",
                        Some(step.name()),
                        actor,
                        Outcome::Success,
                        now,
                    )?;
                }
            }
            let mut next = tenant.clone();
            next.state = TenantState::Deleted;
            next.name_reserved_until = None;
            self.store.update(next, tenant.version)
        })
    }

    // ---- deprovisioning -----------------------------------------------------------------

    fn transition(
        &self,
        id: &TenantId,
        actor: &str,
        now: u64,
        action: &'static str,
        from: TenantState,
        to: TenantState,
    ) -> Result<Tenant> {
        self.audited(id, action, actor, now, || {
            let tenant = self.store.get(id)?;
            if tenant.state != from {
                return Err(Error::WrongState {
                    tenant: id.to_string(),
                    state: tenant.state,
                    operation: action,
                });
            }
            let mut next = tenant.clone();
            next.state = to;
            self.store.update(next, tenant.version)
        })
    }

    /// Soft deprovisioning: `Active -> Suspended`. Data is kept.
    ///
    /// # Errors
    /// Wrong state, audit or store failure.
    pub fn suspend(&self, id: &TenantId, actor: &str, now: u64) -> Result<Tenant> {
        self.transition(
            id,
            actor,
            now,
            "suspend",
            TenantState::Active,
            TenantState::Suspended,
        )
    }

    /// `Suspended -> Active`.
    ///
    /// # Errors
    /// Wrong state, audit or store failure.
    pub fn reactivate(&self, id: &TenantId, actor: &str, now: u64) -> Result<Tenant> {
        self.transition(
            id,
            actor,
            now,
            "reactivate",
            TenantState::Suspended,
            TenantState::Active,
        )
    }

    fn deletion_started(&self, id: &TenantId) -> Result<bool> {
        Ok(self
            .store
            .journal(id)?
            .iter()
            .any(|e| e.kind == JournalKind::Compensated))
    }

    /// Archive: a suspended tenant gets a final backup, which must verify, and the tenant enters
    /// `Deprovisioning` with a retention delay of `retention_ms` (at least the configured minimum).
    ///
    /// # Errors
    /// Wrong state (only `Suspended`), legal hold, retention too short, backup failure or
    /// [`Error::ArchiveUnverified`] (the tenant then stays `Suspended`).
    pub fn archive(
        &self,
        id: &TenantId,
        actor: &str,
        now: u64,
        retention_ms: u64,
    ) -> Result<Tenant> {
        self.audited(id, "archive", actor, now, || {
            let tenant = self.store.get(id)?;
            if tenant.state != TenantState::Suspended {
                return Err(Error::WrongState {
                    tenant: id.to_string(),
                    state: tenant.state,
                    operation: "archive",
                });
            }
            if tenant.flags.contains(TenantFlags::LEGAL_HOLD) {
                return Err(Error::LegalHold {
                    tenant: id.to_string(),
                });
            }
            if retention_ms < self.config.min_retention_ms {
                return Err(Error::InvalidRequest {
                    reason: "the retention delay is shorter than the configured minimum",
                });
            }
            let delete_after = now.checked_add(retention_ms).ok_or(Error::InvalidRequest {
                reason: "the retention delay overflows",
            })?;
            let backup_ref =
                self.archiver
                    .final_backup(&tenant)
                    .map_err(|f| Error::StepFailed {
                        tenant: id.to_string(),
                        step: "final_backup",
                        reason: f.reason,
                    })?;
            match self.archiver.verify(&tenant, &backup_ref) {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    return Err(Error::ArchiveUnverified {
                        tenant: id.to_string(),
                    });
                }
            }
            let mut next = tenant.clone();
            next.state = TenantState::Deprovisioning;
            next.retention = Some(Retention {
                backup_ref,
                verified: true,
                delete_after,
            });
            self.store.update(next, tenant.version)
        })
    }

    /// Cancels a deprovisioning that has not started destroying anything: back to `Suspended`.
    ///
    /// # Errors
    /// Wrong state, or [`Error::WrongState`] once deletion has begun (resources are already gone).
    pub fn cancel_deprovision(&self, id: &TenantId, actor: &str, now: u64) -> Result<Tenant> {
        self.audited(id, "cancel_deprovision", actor, now, || {
            let tenant = self.store.get(id)?;
            if tenant.state != TenantState::Deprovisioning || self.deletion_started(id)? {
                return Err(Error::WrongState {
                    tenant: id.to_string(),
                    state: tenant.state,
                    operation: "cancel deprovisioning",
                });
            }
            let mut next = tenant.clone();
            next.state = TenantState::Suspended;
            next.retention = None;
            self.store.update(next, tenant.version)
        })
    }

    /// Hard deletion after the retention delay, with two-person approval. Destroys the tenant's
    /// resources in reverse provisioning order, marks it `Deleted` and reserves its name.
    /// Repeatable after a failed destruction: finished compensations are skipped.
    ///
    /// # Errors
    /// [`Error::LegalHold`], [`Error::ArchiveUnverified`], [`Error::RetentionPending`],
    /// [`Error::ApprovalRequired`] (fewer than two distinct approvers other than `requester`),
    /// [`Error::AuditFailed`] (intent could not be recorded, nothing destroyed),
    /// [`Error::StepFailed`], wrong state, or a store failure.
    pub fn delete(&self, id: &TenantId, requester: &str, now: u64) -> Result<Tenant> {
        self.audited(id, "delete", requester, now, || {
            let tenant = self.store.get(id)?;
            if tenant.state != TenantState::Deprovisioning {
                return Err(Error::WrongState {
                    tenant: id.to_string(),
                    state: tenant.state,
                    operation: "delete",
                });
            }
            if tenant.flags.contains(TenantFlags::LEGAL_HOLD) {
                return Err(Error::LegalHold {
                    tenant: id.to_string(),
                });
            }
            let retention = match &tenant.retention {
                Some(r) if r.verified => r.clone(),
                _ => {
                    return Err(Error::ArchiveUnverified {
                        tenant: id.to_string(),
                    });
                }
            };
            if now < retention.delete_after {
                return Err(Error::RetentionPending {
                    tenant: id.to_string(),
                    until: retention.delete_after,
                });
            }
            let me = normalise(requester);
            let approvers: BTreeSet<String> = self
                .approvals
                .approvers(&tenant)
                .iter()
                .map(|p| normalise(p))
                .filter(|p| !p.is_empty() && *p != me)
                .collect();
            if approvers.len() < 2 {
                return Err(Error::ApprovalRequired {
                    tenant: id.to_string(),
                });
            }
            if !matches!(
                self.archiver.verify(&tenant, &retention.backup_ref),
                Ok(true)
            ) {
                return Err(Error::ArchiveUnverified {
                    tenant: id.to_string(),
                });
            }
            // Intent first: if it cannot be recorded, nothing is destroyed.
            self.record(id, "delete_begin", None, requester, Outcome::Success, now)?;
            let journal = self.store.journal(id)?;
            for step in Step::ALL.iter().rev().copied() {
                if status(&journal, step) == Status::Compensated {
                    continue;
                }
                if let Err(f) = self.undo(step, &tenant) {
                    let _ = self.record(
                        id,
                        "delete_step",
                        Some(step.name()),
                        requester,
                        Outcome::Failure(codes::STEP_FAILED),
                        now,
                    );
                    return Err(step_failed(id, step, f));
                }
                self.journal(id, step, JournalKind::Compensated, now)?;
                self.record(
                    id,
                    "delete_step",
                    Some(step.name()),
                    requester,
                    Outcome::Success,
                    now,
                )?;
            }
            let mut next = tenant.clone();
            next.state = TenantState::Deleted;
            next.name_reserved_until = Some(now.saturating_add(self.config.name_reservation_ms));
            self.store.update(next, tenant.version)
        })
    }
}
