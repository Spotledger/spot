//! Elevation on the host (WP-EXT-06.T8): the save gate of the two scriptable DocTypes and the
//! audit sink that writes an elevated run onto the site's audit chain.
//!
//! The rules are `spot_server_scripts::elevation`; this file is the host's side of them:
//!
//! - [`ElevationStep`]: the `before_save` step of `server_script` and `api_script`. It asks
//!   the site's permission check whether the saving user holds `scripts_elevate`, applies
//!   [`check_save`](spot_server_scripts::elevation::check_save), stamps `elevated_by` and
//!   `elevated_at`, and tells the user when a changed source reset the flag. The host's own
//!   write-back (a run summary, a disable) never passes through it.
//! - [`ChainElevationAudit`]: an [`ElevationAudit`] over an [`ElevationLog`] port the host binds to
//!   the audit chain appender. `reserve` appends a `reserved` entry before the run starts, so a
//!   chain that cannot be written refuses the run; `record` appends the outcome.
//!
//! Neither carries script text, arguments or values: an entry has the script's name, its kind,
//! the triggering user, the DocType, the operation, the outcome code and two counts.

use super::*;
use spot_server_scripts::elevation::{
    ElevatedRun, ElevationAudit, PERMISSION_ELEVATE, Saver, apply_plan, check_save,
};

/// Whether `user` holds a named app permission on this site (the permission engine's decision;
/// the platform acting as itself is not a user and is never asked here).
pub trait PermissionCheck: Send + Sync {
    /// Whether the invocation's actor holds `permission`.
    fn holds<'a>(&'a self, inv: &'a Invocation, permission: &'a str) -> BoxFuture<'a, bool>;
}

/// The permission check of a site's Server Script runtime, asked late: the controllers are
/// installed before the runtime exists, so the check reads the slot when it is called. While the
/// slot is not ready, or has no check, nobody holds the permission (default deny).
pub struct SlotPermissions {
    slot: Arc<ScriptSlot>,
}

impl SlotPermissions {
    /// A check over `slot`.
    #[must_use]
    pub fn new(slot: Arc<ScriptSlot>) -> SlotPermissions {
        SlotPermissions { slot }
    }
}

impl PermissionCheck for SlotPermissions {
    fn holds<'a>(&'a self, inv: &'a Invocation, permission: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            match self.slot.state() {
                SlotState::Ready(runtime) => runtime.holds(inv, permission).await,
                SlotState::Off | SlotState::Pending => false,
            }
        })
    }
}

/// The save gate step of `doctype` (`server_script` or `api_script`).
pub struct ElevationStep {
    doctype: &'static str,
    scheduled: bool,
    permissions: Arc<dyn PermissionCheck>,
}

impl ElevationStep {
    /// The gate of `doctype`. `scheduled` is true for `scheduled_script`, where the flag is
    /// refused outright.
    #[must_use]
    pub fn new(
        doctype: &'static str,
        scheduled: bool,
        permissions: Arc<dyn PermissionCheck>,
    ) -> ElevationStep {
        ElevationStep {
            doctype,
            scheduled,
            permissions,
        }
    }

    /// Whether `step` is this gate's step.
    #[must_use]
    pub fn owns(&self, step: &StepName) -> bool {
        self.step_name()
            .is_ok_and(|own| own.as_str() == step.as_str())
    }

    /// The step's name: `<doctype>:elevation`.
    ///
    /// # Errors
    ///
    /// Never for the fixed DocType names.
    pub fn step_name(&self) -> AppResult<StepName> {
        StepName::new(&format!("{}:elevation", self.doctype))
    }

    /// The declaration: before the compile step (priority -2,000 < -1,000), so a script whose
    /// flag is refused is never compiled or stored.
    ///
    /// # Errors
    ///
    /// Never for the fixed DocType names.
    pub fn decl(&self) -> AppResult<StepDecl> {
        Ok(StepDecl {
            name: self.step_name()?,
            point: HookPoint::of(Phase::Before, Target::Save),
            priority: -2_000,
            budget: Duration::from_secs(1),
        })
    }

    /// Runs the gate on the document being saved.
    ///
    /// # Errors
    ///
    /// The gate's refusals (`serverscript.elevation_*`).
    pub async fn run(&self, ctx: &mut dyn HookContext) -> AppResult<()> {
        let inv = ctx.invocation().clone();
        // The host's own write-back is not a user's edit.
        if matches!(&inv.actor, Actor::System { .. }) {
            return Ok(());
        }
        let may_elevate = self.permissions.holds(&inv, PERMISSION_ELEVATE).await;
        let user = match &inv.actor {
            Actor::User { id, .. } => id.to_string(),
            other => format!("{other:?}"),
        };
        let at = inv
            .now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let saver = Saver {
            user,
            may_elevate,
            at,
        };
        let id = ctx.doc().id();
        let previous = match ctx.doc().version() {
            Some(_) => ctx.load(self.doctype, id).await.ok(),
            None => None,
        };
        let plan = check_save(ctx.doc(), previous.as_ref(), &saver, self.scheduled)?;
        let reset = plan.reset;
        // A scheduled script has no elevation fields (its DocType does not declare them), and its
        // plan is a no-op: writing them would fail the save as unknown fields.
        if !self.scheduled {
            apply_plan(ctx.doc_mut()?, &plan)?;
        }
        if reset {
            ctx.message(spot_app_api::Message::warning(
                spot_server_scripts::ELEVATION_RESET,
                spot_server_scripts::elevation::RESET_MESSAGE,
            ));
        }
        Ok(())
    }
}

/// Where an elevated run's entries are appended: the host binds it to the site's audit chain.
pub trait ElevationLog: Send + Sync {
    /// Appends one entry. An error means the chain did not take it.
    fn append<'a>(&'a self, entry: ElevationEntry) -> BoxFuture<'a, AppResult<()>>;
}

/// One audit chain entry of an elevated run: names, codes and counts only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElevationEntry {
    /// `reserved` or `finished`.
    pub stage: &'static str,
    /// The run.
    pub run: ElevatedRun,
}

/// [`ElevationAudit`] over an [`ElevationLog`].
pub struct ChainElevationAudit {
    log: Arc<dyn ElevationLog>,
}

impl ChainElevationAudit {
    /// An audit writing to `log`.
    #[must_use]
    pub fn new(log: Arc<dyn ElevationLog>) -> ChainElevationAudit {
        ChainElevationAudit { log }
    }
}

impl ElevationAudit for ChainElevationAudit {
    fn reserve<'a>(&'a self, run: &'a ElevatedRun) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.log
                .append(ElevationEntry {
                    stage: "reserved",
                    run: run.clone(),
                })
                .await
                .map_err(audit_failed)
        })
    }

    fn record<'a>(&'a self, run: &'a ElevatedRun) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.log
                .append(ElevationEntry {
                    stage: "finished",
                    run: run.clone(),
                })
                .await
                .map_err(audit_failed)
        })
    }
}

fn audit_failed(_cause: AppError) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        spot_server_scripts::ELEVATION_AUDIT_FAILED,
        "The elevated run could not be audited, so it was not allowed.",
    )
}
