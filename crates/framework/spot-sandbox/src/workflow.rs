//! The checked sandbox lifecycle.
//!
//! [`Sandbox`] is the only thing that decides whether a sandbox is reachable. It starts closed,
//! runs snapshot → restore into a **closed** tenant → mask (with the outbound lock installed) →
//! leak scan → activate, and opens the tenant in exactly one place: [`Sandbox::activate`],
//! after a clean [`ScanReport`].
//!
//! Every step is refused in the wrong state with `sandbox.state`, so a caller cannot skip the
//! scan, mask twice or activate a quarantined sandbox. Time is an argument everywhere:
//! `at` is Unix microseconds, `now_ms` is the millisecond timestamp the tenant id needs.

use spot_tenancy::TenantId;

use crate::MaskingEngine;
use crate::error::{Error, Result};
use crate::mask::{MaskReport, SandboxKey};
use crate::plan::{FieldPlan, SandboxPlan, SnapshotRef};
use crate::ports::{EffectPort, OutboundLock, SnapshotPort, StorePort, TenantPort, TenantStatus};
use crate::scan::{ScanReport, SourceIndex, scan};

/// Where a sandbox is in its lifecycle. Only [`SandboxState::Active`] admits users.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SandboxState {
    /// The plan is validated; nothing has happened yet.
    Planned,
    /// A snapshot was taken (or pinned) and belongs to the source tenant.
    SnapshotTaken,
    /// Restored into a new, closed tenant.
    Restored,
    /// Masking is running; the gate is closed.
    Masking,
    /// Masked; a leak scan has not run (or found leaks and is being re-run).
    Masked,
    /// The leak scan is running; the gate is closed.
    Scanning,
    /// The leak scan was clean; activation may follow.
    Verified,
    /// The leak scan found an unmasked value; activation is refused.
    Quarantined,
    /// Activated: the tenant admits users and every outbound effect is locked off.
    Active,
    /// A step failed; the sandbox stays closed and can be inspected.
    Failed,
}

impl SandboxState {
    /// The state's stable name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxState::Planned => "planned",
            SandboxState::SnapshotTaken => "snapshot_taken",
            SandboxState::Restored => "restored",
            SandboxState::Masking => "masking",
            SandboxState::Masked => "masked",
            SandboxState::Scanning => "scanning",
            SandboxState::Verified => "verified",
            SandboxState::Quarantined => "quarantined",
            SandboxState::Active => "active",
            SandboxState::Failed => "failed",
        }
    }

    /// Whether a tenant in this state may admit users. Exactly one state says yes.
    #[must_use]
    pub fn is_reachable(self) -> bool {
        matches!(self, SandboxState::Active)
    }
}

/// What became of the tenant after a failed run, for the administrator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cleanup {
    /// The tenant was marked unusable and then destroyed.
    Destroyed,
    /// The tenant was marked unusable but could not be destroyed: it stays visible as
    /// unusable until the administrator removes it.
    LeftUnusable,
    /// The control plane could not even mark the tenant unusable. The workflow returned an
    /// error saying so; the tenant is closed but its registry status is not `Unusable`.
    NotMarked,
}

/// One step of the lifecycle, for the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// Take or verify the production snapshot.
    Snapshot,
    /// Provision the closed tenant and restore into it.
    Restore,
    /// Install the outbound lock and mask.
    Mask,
    /// Scan for unmasked source values.
    Scan,
    /// Open the tenant.
    Activate,
}

impl Step {
    /// The step's stable name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Snapshot => "snapshot",
            Step::Restore => "restore",
            Step::Mask => "mask",
            Step::Scan => "scan",
            Step::Activate => "activate",
        }
    }
}

/// What happened, when, and whether it succeeded. It holds no value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepRecord {
    /// The step.
    pub step: Step,
    /// Unix microseconds.
    pub at: i64,
    /// Whether the step completed.
    pub ok: bool,
}

/// A sandbox lifecycle in progress.
#[derive(Debug)]
pub struct Sandbox {
    plan: SandboxPlan,
    state: SandboxState,
    snapshot: Option<SnapshotRef>,
    tenant: Option<TenantId>,
    mask_report: Option<MaskReport>,
    field_plan: Option<FieldPlan>,
    cleanup: Option<Cleanup>,
    index: Option<SourceIndex>,
    last_scan: Option<ScanReport>,
    lock: Option<OutboundLock>,
    steps: Vec<StepRecord>,
}

impl Sandbox {
    /// A closed sandbox for a validated plan.
    ///
    /// # Errors
    /// [`Error::InvalidPlan`] when the plan does not validate.
    pub fn new(plan: SandboxPlan) -> Result<Self> {
        plan.validate()?;
        Ok(Self {
            plan,
            state: SandboxState::Planned,
            snapshot: None,
            tenant: None,
            mask_report: None,
            field_plan: None,
            cleanup: None,
            index: None,
            last_scan: None,
            lock: None,
            steps: Vec::new(),
        })
    }

    /// The plan.
    #[must_use]
    pub fn plan(&self) -> &SandboxPlan {
        &self.plan
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> SandboxState {
        self.state
    }

    /// Whether the tenant admits users. `false` until [`Sandbox::activate`] succeeds.
    #[must_use]
    pub fn is_reachable(&self) -> bool {
        self.state.is_reachable()
    }

    /// The tenant, once provisioned.
    #[must_use]
    pub fn tenant(&self) -> Option<TenantId> {
        self.tenant
    }

    /// The snapshot in use.
    #[must_use]
    pub fn snapshot(&self) -> Option<&SnapshotRef> {
        self.snapshot.as_ref()
    }

    /// The masking report, once masking finished.
    #[must_use]
    pub fn mask_report(&self) -> Option<&MaskReport> {
        self.mask_report.as_ref()
    }

    /// Every field and its class, once masking started. It lists a masking rule or an explicit
    /// Keep decision for each field.
    #[must_use]
    pub fn field_plan(&self) -> Option<&FieldPlan> {
        self.field_plan.as_ref()
    }

    /// What became of the tenant after a failure or a leaky scan, if either happened.
    #[must_use]
    pub fn cleanup(&self) -> Option<Cleanup> {
        self.cleanup
    }

    /// The last leak scan, if one ran.
    #[must_use]
    pub fn last_scan(&self) -> Option<&ScanReport> {
        self.last_scan.as_ref()
    }

    /// The outbound lock, once installed.
    #[must_use]
    pub fn outbound_lock(&self) -> Option<&OutboundLock> {
        self.lock.as_ref()
    }

    /// Every step attempted, in order.
    #[must_use]
    pub fn steps(&self) -> &[StepRecord] {
        &self.steps
    }

    /// Takes the production snapshot, or adopts the plan's pinned snapshot after checking that
    /// it belongs to the source tenant.
    ///
    /// # Errors
    /// [`Error::State`] when not planned, and [`Error::Snapshot`] when the provider fails or
    /// returns a snapshot of another tenant.
    pub fn take_snapshot(&mut self, port: &dyn SnapshotPort, at: i64) -> Result<SnapshotRef> {
        self.require(Step::Snapshot, &[SandboxState::Planned])?;
        let snapshot = match &self.plan.snapshot {
            Some(pinned) => pinned.clone(),
            None => port.take(&self.plan.source_tenant, at)?,
        };
        if snapshot.source_tenant != self.plan.source_tenant {
            self.fail(Step::Snapshot, at);
            return Err(Error::Snapshot {
                reason: "the provider returned a snapshot of another tenant".to_owned(),
            });
        }
        self.snapshot = Some(snapshot.clone());
        self.state = SandboxState::SnapshotTaken;
        self.record(Step::Snapshot, at, true);
        Ok(snapshot)
    }

    /// Provisions the closed sandbox tenant and restores the snapshot into it. The tenant
    /// reaches the control plane **closed**; only [`Sandbox::activate`] opens it.
    ///
    /// # Errors
    /// [`Error::State`] when no snapshot is taken, [`Error::Tenant`] when provisioning fails
    /// and [`Error::Restore`] when the restore fails.
    pub fn restore(
        &mut self,
        snapshots: &dyn SnapshotPort,
        tenants: &dyn TenantPort,
        now_ms: u64,
        entropy: [u8; 10],
        at: i64,
    ) -> Result<TenantId> {
        self.require(Step::Restore, &[SandboxState::SnapshotTaken])?;
        let Some(snapshot) = self.snapshot.clone() else {
            return Err(Error::State {
                step: Step::Restore.as_str(),
                state: self.state.as_str().to_owned(),
            });
        };
        let tenant = tenants.provision(&self.plan.sandbox_tenant, now_ms, entropy)?;
        // From here the tenant exists and may hold copied data: keep its id so a failure can
        // quarantine it.
        self.tenant = Some(tenant);
        if let Err(error) = snapshots.restore(&snapshot, &self.plan.sandbox_tenant, at) {
            return Err(self.fail_closed(tenants, Step::Restore, at, error));
        }
        self.state = SandboxState::Restored;
        self.record(Step::Restore, at, true);
        Ok(tenant)
    }

    /// Installs the outbound lock, masks every surface and keeps the source index for the scan.
    ///
    /// # Errors
    /// [`Error::State`] when not restored, [`Error::Effects`] when the lock cannot be
    /// installed, and [`Error::Mask`]/[`Error::Store`] from the engine.
    pub fn mask(
        &mut self,
        engine: &MaskingEngine<'_>,
        store: &mut dyn crate::SandboxStore,
        effects: &dyn EffectPort,
        tenants: &dyn TenantPort,
        key: SandboxKey,
        at: i64,
    ) -> Result<()> {
        self.require(Step::Mask, &[SandboxState::Restored])?;
        // Masking a store of another tenant (production, say) is refused before it is touched.
        self.require_bound(store.tenant_id().as_str())?;
        self.state = SandboxState::Masking;
        let lock = match effects.lock(&self.plan.sandbox_tenant, &self.plan.source_tenant) {
            Ok(lock) => lock,
            Err(error) => return Err(self.fail_closed(tenants, Step::Mask, at, error)),
        };
        let outcome = match engine.mask(store, key) {
            Ok(outcome) => outcome,
            Err(error) => return Err(self.fail_closed(tenants, Step::Mask, at, error)),
        };
        self.lock = Some(lock);
        self.mask_report = Some(outcome.report);
        self.field_plan = Some(outcome.field_plan);
        self.index = Some(outcome.index);
        self.state = SandboxState::Masked;
        self.record(Step::Mask, at, true);
        Ok(())
    }

    /// Scans the masked tenant for any original value of a masked field. The store handle is
    /// opened here, for the sandbox tenant, from `stores`: a caller cannot point the scan at
    /// another store. A leaky scan marks the tenant unusable and destroys it (best effort).
    ///
    /// # Errors
    /// [`Error::State`] when masking has not finished or the scan already found a leak,
    /// [`Error::ScanMissing`] when no source value was indexed, [`Error::Tenant`] when the
    /// handle is unbound or bound to another tenant, or the report names another tenant, or a
    /// leaky tenant could not be marked unusable, and [`Error::Store`] when the store cannot
    /// be opened.
    pub fn scan(
        &mut self,
        stores: &dyn StorePort,
        tenants: &dyn TenantPort,
        at: i64,
    ) -> Result<ScanReport> {
        self.require(Step::Scan, &[SandboxState::Masked, SandboxState::Verified])?;
        let Some(index) = self.index.clone() else {
            return Err(Error::ScanMissing);
        };
        // An empty index proves nothing: a scan that looks for no value always passes.
        if index.is_empty() {
            return Err(Error::ScanMissing);
        }
        let Some(tenant) = self.tenant else {
            return Err(Error::Tenant {
                reason: "no tenant to scan".to_owned(),
            });
        };
        let handle = stores.open(&tenant, &self.plan.sandbox_tenant)?;
        self.require_bound(handle.tenant_id().as_str())?;
        self.state = SandboxState::Scanning;
        let report = scan(handle.as_ref(), &index);
        // The report must name the sandbox tenant; the workflow never fills it in.
        if report.tenant != self.plan.sandbox_tenant {
            let error = Error::Tenant {
                reason: "the scan report names another tenant than the sandbox".to_owned(),
            };
            return Err(self.fail_closed(tenants, Step::Scan, at, error));
        }
        self.last_scan = Some(report.clone());
        if report.is_clean() {
            self.state = SandboxState::Verified;
            self.record(Step::Scan, at, true);
            return Ok(report);
        }
        self.state = SandboxState::Quarantined;
        self.record(Step::Scan, at, true);
        if let Some(error) = self.quarantine_tenant(tenants) {
            return Err(error);
        }
        Ok(report)
    }

    /// Opens the tenant: the only transition to [`SandboxState::Active`]. It requires a clean
    /// scan, an installed outbound lock that denies every destination, and a tenant that is
    /// still closed.
    ///
    /// # Errors
    /// [`Error::State`] when the sandbox has not been scanned, [`Error::Leak`] when the last
    /// scan was not clean, [`Error::Effects`] when the lock is missing or too weak, and
    /// [`Error::Tenant`] when the control plane refuses to open the tenant.
    pub fn activate(&mut self, tenants: &dyn TenantPort, at: i64) -> Result<()> {
        self.require(
            Step::Activate,
            &[SandboxState::Verified, SandboxState::Quarantined],
        )?;
        let clean = self.last_scan.as_ref().is_some_and(ScanReport::is_clean);
        if !clean {
            return Err(match self.last_scan.as_ref() {
                Some(report) => Error::Leak {
                    leaks: report.leaks.len(),
                },
                None => Error::Leak { leaks: 0 },
            });
        }
        let Some(lock) = &self.lock else {
            return Err(Error::Effects {
                reason: "no outbound lock is installed".to_owned(),
            });
        };
        lock.holds_forever()?;
        if !lock.denies_all() {
            return Err(Error::Effects {
                reason: "the egress policy still admits a destination".to_owned(),
            });
        }
        let Some(tenant) = self.tenant else {
            return Err(Error::Tenant {
                reason: "no tenant to open".to_owned(),
            });
        };
        let scanned_here = self
            .last_scan
            .as_ref()
            .is_some_and(|report| report.tenant == self.plan.sandbox_tenant);
        if !scanned_here {
            return Err(Error::Tenant {
                reason: "the clean scan is not a scan of the sandbox tenant".to_owned(),
            });
        }
        if tenants.is_reachable(&tenant) {
            return Err(Error::Tenant {
                reason: "the tenant was reachable before activation".to_owned(),
            });
        }
        // Open only a tenant the registry still holds as provisioning: an unusable or
        // destroyed tenant is refused here and again by the port.
        if tenants.status(&tenant) != Some(TenantStatus::Provisioning) {
            return Err(Error::Tenant {
                reason: "the tenant is not provisioning and cannot be opened".to_owned(),
            });
        }
        tenants.mark_ready(&tenant)?;
        if tenants.status(&tenant) != Some(TenantStatus::Ready) {
            return Err(Error::Tenant {
                reason: "the tenant did not become ready".to_owned(),
            });
        }
        tenants.set_reachable(&tenant, true)?;
        self.state = SandboxState::Active;
        self.record(Step::Activate, at, true);
        Ok(())
    }

    fn require(&self, step: Step, allowed: &[SandboxState]) -> Result<()> {
        if allowed.contains(&self.state) {
            Ok(())
        } else {
            Err(Error::State {
                step: step.as_str(),
                state: self.state.as_str().to_owned(),
            })
        }
    }

    /// Refuses a store handle that is unbound or bound to a tenant other than the sandbox.
    fn require_bound(&self, bound: &str) -> Result<()> {
        if bound.is_empty() || bound != self.plan.sandbox_tenant {
            return Err(Error::Tenant {
                reason: "the store is not bound to the sandbox tenant".to_owned(),
            });
        }
        Ok(())
    }

    /// A step failed after the copy started: record it, mark the tenant unusable first, then
    /// destroy it. Returns the error to hand to the caller: `cause`, or a tenant error when
    /// the tenant could not even be marked unusable.
    fn fail_closed(
        &mut self,
        tenants: &dyn TenantPort,
        step: Step,
        at: i64,
        cause: Error,
    ) -> Error {
        self.fail(step, at);
        match self.quarantine_tenant(tenants) {
            Some(error) => Error::Tenant {
                reason: format!(
                    "{}; the failed tenant could not be marked unusable (cause {})",
                    error,
                    cause.code()
                ),
            },
            None => cause,
        }
    }

    /// Marks the tenant unusable, then tries to destroy it, and records the outcome. Returns
    /// an error only when the tenant could not be marked unusable.
    fn quarantine_tenant(&mut self, tenants: &dyn TenantPort) -> Option<Error> {
        let tenant = self.tenant?;
        let marked = tenants.mark_unusable(&tenant);
        let destroyed = tenants.destroy(&tenant);
        match (marked, destroyed) {
            (Ok(()) | Err(_), Ok(())) => {
                self.cleanup = Some(Cleanup::Destroyed);
                None
            }
            (Ok(()), Err(_)) => {
                self.cleanup = Some(Cleanup::LeftUnusable);
                None
            }
            (Err(error), Err(_)) => {
                self.cleanup = Some(Cleanup::NotMarked);
                Some(error)
            }
        }
    }

    fn fail(&mut self, step: Step, at: i64) {
        self.state = SandboxState::Failed;
        self.record(step, at, false);
    }

    fn record(&mut self, step: Step, at: i64, ok: bool) {
        self.steps.push(StepRecord { step, at, ok });
    }
}
