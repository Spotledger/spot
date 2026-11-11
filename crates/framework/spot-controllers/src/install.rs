//! Attaching controllers to the seam registry (ADR-51).
//!
//! The engine asks [`spot_seams::Registry`] which steps run at each hook point; it has no idea a
//! controller exists. [`ControllerSet::install_into`](crate::ControllerSet::install_into) is the
//! bridge: it registers every controller's steps, with the app manifest's priority and budget
//! when the manifest names the step and the step's own declaration otherwise, and installs the
//! app's overrides. Conflict detection is `spot-seams`' job and reports every conflict at once
//! when the host builds the registry.
//!
//! A manifest that names a step the controller does not declare, or names it at another hook
//! point, changes only the steps the controller actually declares; the controller stays the
//! source of truth for what it can run.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as Json;
use spot_app_api::{
    AppResult, BoxFuture, Controller, HookContext, MethodContext, MethodDecl, StepDecl,
    StepEffects, StepName,
};

/// A controller whose steps carry the app manifest's priority and budget.
///
/// `steps` is the controller's own list with the manifest's point, priority and budget applied
/// to every step the manifest names. Everything else delegates to the wrapped controller.
pub(crate) struct AdjustedController {
    inner: Arc<dyn Controller>,
    steps: Vec<StepDecl>,
}

impl AdjustedController {
    /// Wraps `inner`, serving `steps`.
    pub(crate) fn new(inner: Arc<dyn Controller>, steps: Vec<StepDecl>) -> AdjustedController {
        AdjustedController { inner, steps }
    }
}

impl Controller for AdjustedController {
    fn doctype(&self) -> &str {
        self.inner.doctype()
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.steps.clone()
    }

    /// Forwarded unchanged: the manifest adjusts a step's point, priority and budget, never its
    /// name, so the effects stay keyed to the same steps. Without this the trait default (no
    /// effects) would hide every installed controller's declarations from the strict check.
    fn step_effects(&self) -> Vec<StepEffects> {
        self.inner.step_effects()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        self.inner.methods()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        self.inner.run_step(step, ctx)
    }

    fn call_method<'a>(
        &'a self,
        method: &'a str,
        args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        self.inner.call_method(method, args, ctx)
    }
}

/// The controller's steps with the manifest's point, priority and budget for each named step.
pub(crate) fn adjusted_steps(
    controller: &Arc<dyn Controller>,
    manifest: Option<&BTreeMap<(String, String), StepDecl>>,
) -> Vec<StepDecl> {
    let mut steps = controller.steps();
    if let Some(declared) = manifest {
        for step in &mut steps {
            if let Some(decl) = declared.get(&(
                controller.doctype().to_owned(),
                step.name.as_str().to_owned(),
            )) {
                step.point = decl.point.clone();
                step.priority = decl.priority;
                step.budget = decl.budget;
            }
        }
    }
    steps
}
