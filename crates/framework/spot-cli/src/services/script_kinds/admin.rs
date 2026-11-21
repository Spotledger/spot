//! The administration controllers of the two DocTypes (WP-EXT-06.T7): compile on save, and a
//! reload once a save, change or delete commits.

use super::*;
use spot_server_scripts::kinds::{API_SCRIPT_DOCTYPE, SCHEDULED_SCRIPT_DOCTYPE};

/// The controller of the `api_script` or `scheduled_script` DocType.
pub struct KindAdmin {
    kind: ScriptKind,
    compile: KindCompileController,
    elevation: ElevationStep,
    handle: Arc<KindsHandle>,
}

/// The permission check of a site's script kinds, asked late: the controllers are installed
/// before the runtime exists, so the check reads the handle when it is called. While the module
/// is not ready, or has no check, nobody holds the permission (default deny).
struct HandlePermissions(Arc<KindsHandle>);

impl PermissionCheck for HandlePermissions {
    fn holds<'a>(&'a self, inv: &'a Invocation, permission: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            match self.0.runtime() {
                Some(runtime) => runtime.holds(inv, permission).await,
                None => false,
            }
        })
    }
}

impl KindAdmin {
    /// The controller of `kind` over `handle`'s runtime and `engine` (it compiles on save).
    #[must_use]
    pub fn new(kind: ScriptKind, handle: Arc<KindsHandle>, engine: spot_ts::ScriptEngine) -> Self {
        let (doctype, scheduled) = match kind {
            ScriptKind::Api => (API_SCRIPT_DOCTYPE, false),
            // A scheduled script can never be elevated: the gate refuses the flag outright.
            ScriptKind::Scheduled => (SCHEDULED_SCRIPT_DOCTYPE, true),
        };
        KindAdmin {
            kind,
            compile: KindCompileController::new(kind, engine, Arc::new(NoStore)),
            elevation: ElevationStep::new(
                doctype,
                scheduled,
                Arc::new(HandlePermissions(Arc::clone(&handle))),
            ),
            handle,
        }
    }

    fn step(&self, suffix: &str) -> Option<StepName> {
        StepName::new(&format!("{}:{suffix}", self.kind.doctype())).ok()
    }
}

/// Whether the invocation is this module's own write-back (a run summary, a disable), which does
/// not reload on top of the explicit refresh the writer asks for.
fn is_own(inv: &Invocation) -> bool {
    matches!(&inv.actor, Actor::System { component } if component.as_str() == COMPONENT)
}

impl Controller for KindAdmin {
    fn doctype(&self) -> &str {
        self.kind.doctype()
    }

    fn steps(&self) -> Vec<StepDecl> {
        let mut steps = self.compile.steps();
        // The elevation gate runs before the compile step, so a refused flag is never compiled.
        steps.extend(self.elevation.decl().ok());
        for (suffix, point) in [
            (
                "reload_saved",
                HookPoint::of(Phase::Committed, Target::Save),
            ),
            (
                "reload_deleted",
                HookPoint::of(Phase::Committed, Target::Op(Operation::Delete)),
            ),
        ] {
            if let Some(name) = self.step(suffix) {
                steps.push(StepDecl {
                    name,
                    point,
                    priority: 0,
                    budget: Duration::from_secs(5),
                });
            }
        }
        steps
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if self.elevation.owns(step) {
                return self.elevation.run(ctx).await;
            }
            let name = step.as_str();
            if self
                .step("reload_saved")
                .is_some_and(|s| s.as_str() == name)
                || self
                    .step("reload_deleted")
                    .is_some_and(|s| s.as_str() == name)
            {
                if !is_own(ctx.invocation()) {
                    self.handle.reload().await;
                }
                return Ok(());
            }
            self.compile.run_step(step, ctx).await
        })
    }
}

/// The store the compile step records through: it keeps nothing (the hash goes on the document
/// being saved).
struct NoStore;

impl KindStore for NoStore {
    fn record_compile<'a>(
        &'a self,
        _kind: ScriptKind,
        _name: &'a str,
        _outcome: CompileOutcome,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn record_run<'a>(&'a self, _run: KindRun) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn disable<'a>(
        &'a self,
        _kind: ScriptKind,
        _name: &'a str,
        _reason: &'a str,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Registers the two admin controllers into `registry`. Call it once per site build, before the
/// registry is installed (the engine's seam registry cannot be extended afterwards), only for a
/// site that has the `scripting` app.
pub fn register_controllers(registry: &mut ControllerRegistry, site: &str) {
    let handle = handle(site);
    let engine = crate::services::scripts::slot(site).script_engine().clone();
    for kind in [ScriptKind::Api, ScriptKind::Scheduled] {
        registry.register(
            kind.doctype(),
            Arc::new(KindAdmin::new(kind, Arc::clone(&handle), engine.clone())),
            ControllerSource::TypeScript,
        );
    }
}
