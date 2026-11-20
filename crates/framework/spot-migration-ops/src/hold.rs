//! Owned by WP-MIG-08.T4 (hold and release).
//!
//! The side-effect hold as lifecycle operations: `hold`, `release` (replay, discard or selective)
//! and `status`, and the handlers of `site.migrate.hold` and `site.migrate.release`.
//!
//! # Rules
//!
//! * Every operation needs the `migration.run` permission of the actor (the right apply needs).
//! * A hold taken here uses the migration id [`MANUAL_HOLD_ID`]; a hold taken by a running
//!   migration carries that migration's id and is released by the migration, not here: releasing
//!   it through this module is `migration.hold_active`.
//! * `replay` performs the held effects in their original order, each with its original
//!   idempotency key (the hold service's release). `discard` drops them and needs a **second
//!   approver**: another principal, who also holds the permission, named in the request. A
//!   discard is always an explicit, audited decision; the hold service records the class and key
//!   digest of every effect it drops.
//! * `selective` (release only some classes) is not supported by the [`SideEffectHold`] port and
//!   is refused with `migration.not_implemented`; it never falls back to a full replay.
//! * Errors never carry the hold service's message (it can name a migration id or a key).

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_mgmt_ops::{OpId, Validated};
use spot_migration_apply::BoxFuture;
use spot_permissions::Principal;

use crate::catalogue::{HandlerOutput, HandlerRegistry, OperationHandler};
use crate::context::{AuditSpec, LifecycleContext, no_digest, no_plan};
use crate::error::LifecycleError;

/// The migration id of a hold taken by an operator, not by a migration.
pub const MANUAL_HOLD_ID: &str = "manual";

/// The reason recorded when a discard is requested without one.
pub const DISCARD_DEFAULT_REASON: &str = "discarded by an operator";

/// What `release` does with the held effects.
#[derive(Clone, Debug)]
pub enum ReleaseMode {
    /// Perform them, once, in order.
    Replay,
    /// Drop them. Needs a second approver who also holds the permission.
    Discard {
        /// The second approver (not the actor).
        approver: Principal,
        /// Why (recorded in the audit).
        reason: String,
    },
    /// Perform only the named effect classes. Not supported by the port: refused.
    Selective {
        /// The class keys.
        classes: Vec<String>,
    },
}

/// What `hold` and `release` report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldOutcome {
    /// The migration id the hold carries.
    pub migration_id: String,
    /// The time the operation ran (the context's clock).
    pub at: i64,
}

/// The hold as the ledger knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldView {
    /// Whether the site is held.
    pub held: bool,
    /// The migration id of the live hold, when held.
    pub migration_id: Option<String>,
    /// The reason, when held.
    pub reason: Option<String>,
    /// When the hold began, when held.
    pub since: Option<i64>,
}

/// Reads the hold state (the [`SideEffectHold`] port has no read; the host provides this).
///
/// [`SideEffectHold`]: spot_migration_apply::SideEffectHold
pub trait HoldInspector: Send + Sync {
    /// The current state. An unreadable state is an error, never "open".
    fn state(&self) -> BoxFuture<'_, Result<HoldView, LifecycleError>>;
}

/// Begins a hold under `scope` (the migration id; [`MANUAL_HOLD_ID`] for an operator's hold).
///
/// # Errors
/// `migration.not_permitted`; `migration.hold_active` when a hold is already live under another
/// id or the ledger cannot be written (the site then stays as it was).
pub async fn hold(
    ctx: &LifecycleContext,
    scope: &str,
    reason: &str,
) -> Result<HoldOutcome, LifecycleError> {
    ctx.audited(AuditSpec::new("hold", no_digest(), no_plan()), |_| {
        hold_body(ctx, scope, reason)
    })
    .await
}

async fn hold_body(
    ctx: &LifecycleContext,
    scope: &str,
    reason: &str,
) -> Result<HoldOutcome, LifecycleError> {
    ctx.require_permission()?;
    let scope = valid_scope(scope)?;
    let at = ctx.now();
    ctx.hold()
        .begin(scope, reason, at)
        .await
        .map_err(|_| LifecycleError::HoldActive)?;
    Ok(HoldOutcome {
        migration_id: scope.to_owned(),
        at,
    })
}

/// Releases the hold of `scope`.
///
/// # Errors
/// `migration.not_permitted` (also for a missing, identical or unpermitted second approver on a
/// discard); `migration.not_implemented` for `selective`; `migration.hold_active` when the
/// release failed and the site stays held.
pub async fn release(
    ctx: &LifecycleContext,
    scope: &str,
    mode: ReleaseMode,
) -> Result<HoldOutcome, LifecycleError> {
    ctx.audited(AuditSpec::new("release", no_digest(), no_plan()), |_| {
        release_body(ctx, scope, mode)
    })
    .await
}

async fn release_body(
    ctx: &LifecycleContext,
    scope: &str,
    mode: ReleaseMode,
) -> Result<HoldOutcome, LifecycleError> {
    ctx.require_permission()?;
    let scope = valid_scope(scope)?;
    let at = ctx.now();
    match mode {
        ReleaseMode::Replay => ctx
            .hold()
            .release(scope, at)
            .await
            .map_err(|_| LifecycleError::HoldActive)?,
        ReleaseMode::Discard { approver, reason } => {
            check_second_approver(ctx, &approver)?;
            let reason = if reason.trim().is_empty() {
                DISCARD_DEFAULT_REASON
            } else {
                reason.as_str()
            };
            ctx.hold()
                .discard(scope, reason, at)
                .await
                .map_err(|_| LifecycleError::HoldActive)?;
        }
        ReleaseMode::Selective { .. } => {
            return Err(LifecycleError::NotImplemented {
                op: "site.migrate.release --selective".to_owned(),
            });
        }
    }
    Ok(HoldOutcome {
        migration_id: scope.to_owned(),
        at,
    })
}

/// The hold state, through the host's inspector.
///
/// # Errors
/// `migration.not_permitted`; `migration.store_failed` when the state cannot be read.
pub async fn status(
    ctx: &LifecycleContext,
    inspector: &dyn HoldInspector,
) -> Result<HoldView, LifecycleError> {
    ctx.require_permission()?;
    inspector.state().await
}

fn valid_scope(scope: &str) -> Result<&str, LifecycleError> {
    // Printable, non-empty, bounded; the request-scope prefix belongs to the effect-hold crate.
    let ok = !scope.is_empty()
        && scope.len() <= 128
        && !scope.chars().any(char::is_control)
        && !scope.starts_with("request:");
    if ok {
        Ok(scope)
    } else {
        Err(LifecycleError::HoldActive)
    }
}

fn check_second_approver(
    ctx: &LifecycleContext,
    approver: &Principal,
) -> Result<(), LifecycleError> {
    let same_person = !approver.is_system
        && !ctx.actor().principal().is_system
        && approver.user == ctx.actor().principal().user;
    let anonymous = !approver.is_system && approver.user.trim().is_empty();
    // A system principal cannot be the second approver of a person's discard, and nobody can
    // approve their own discard.
    if same_person || anonymous || approver.is_system || !ctx.permits(approver) {
        return Err(LifecycleError::NotPermitted);
    }
    Ok(())
}

fn param(call: &Validated, name: &str) -> Option<String> {
    call.params
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| match &p.value {
            spot_mgmt_ops::ParamValue::Text(text) => Some(text.clone()),
            _ => None,
        })
}

fn same_site(ctx: &LifecycleContext, call: &Validated) -> Result<(), LifecycleError> {
    match param(call, "site") {
        Some(site) if site == ctx.site().as_str() => Ok(()),
        _ => Err(LifecycleError::NotPermitted),
    }
}

fn outcome(outcome: HoldOutcome) -> HandlerOutput {
    let mut fields = BTreeMap::new();
    fields.insert("migration_id".to_owned(), outcome.migration_id.clone());
    fields.insert("at".to_owned(), outcome.at.to_string());
    HandlerOutput {
        message: format!("hold {} done", outcome.migration_id),
        fields,
    }
}

struct HoldHandler(Arc<LifecycleContext>);

impl OperationHandler for HoldHandler {
    fn handle<'a>(
        &'a self,
        call: &'a Validated,
    ) -> BoxFuture<'a, Result<HandlerOutput, LifecycleError>> {
        Box::pin(async move {
            same_site(&self.0, call)?;
            let reason = param(call, "reason").ok_or(LifecycleError::NotPermitted)?;
            hold(&self.0, MANUAL_HOLD_ID, &reason).await.map(outcome)
        })
    }
}

struct ReleaseHandler(Arc<LifecycleContext>);

impl OperationHandler for ReleaseHandler {
    fn handle<'a>(
        &'a self,
        call: &'a Validated,
    ) -> BoxFuture<'a, Result<HandlerOutput, LifecycleError>> {
        Box::pin(async move {
            same_site(&self.0, call)?;
            // The management operation replays; a discard needs a named second approver and
            // goes through `release` with `ReleaseMode::Discard`.
            release(&self.0, MANUAL_HOLD_ID, ReleaseMode::Replay)
                .await
                .map(outcome)
        })
    }
}

/// Registers the handlers of `site.migrate.hold` and `site.migrate.release`.
pub fn register(ctx: &Arc<LifecycleContext>, registry: &mut HandlerRegistry) {
    if let Some(id) = OpId::new("site.migrate.hold") {
        registry.register(id, Arc::new(HoldHandler(Arc::clone(ctx))));
    }
    if let Some(id) = OpId::new("site.migrate.release") {
        registry.register(id, Arc::new(ReleaseHandler(Arc::clone(ctx))));
    }
}
