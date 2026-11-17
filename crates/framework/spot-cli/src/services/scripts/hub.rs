//! The hub (nine fixed steps per DocType) and the Server Script administration controller (WP-EXT-06.T4).

use super::*;


pub(super) const STEP_PREFIX: &str = "server_script:run_";

/// The step of `event` on `doctype`. Step names are unique across the registry, so each DocType's
/// hub carries its own: `server_script:run_<doctype>_<event>`.
pub(super) fn hub_step(doctype: &str, event: ScriptEvent) -> AppResult<StepName> {
    StepName::new(&format!("{STEP_PREFIX}{doctype}_{}", event.as_str()))
}

pub(super) fn not_ready() -> AppError {
    AppError::new(
        ErrorClass::Internal,
        NOT_READY,
        "Server Scripts are not loaded yet; the document was not saved.",
    )
}

/// One DocType's Server Scripts as a controller of nine steps.
pub struct ScriptHub {
    doctype: String,
    slot: Arc<ScriptSlot>,
    budget: Duration,
}

impl ScriptHub {
    /// The hub of `doctype`. `budget` is [`Settings::step_budget`].
    #[must_use]
    pub fn new(doctype: &str, slot: Arc<ScriptSlot>, budget: Duration) -> ScriptHub {
        ScriptHub {
            doctype: doctype.to_owned(),
            slot,
            budget,
        }
    }
}

impl Controller for ScriptHub {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        ScriptEvent::ALL
            .into_iter()
            .filter_map(|event| {
                Some(StepDecl {
                    name: hub_step(&self.doctype, event).ok()?,
                    point: event.hook_point(),
                    priority: SCRIPT_PRIORITY_OFFSET.saturating_add(event.base_priority()),
                    budget: self.budget,
                })
            })
            .collect()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let Some(event) = ScriptEvent::ALL
                .into_iter()
                .find(|event| hub_step(&self.doctype, *event).is_ok_and(|name| &name == step))
            else {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "controller.no_step",
                    format!("The script hub has no step `{step}`."),
                ));
            };
            let runtime = match self.slot.state() {
                SlotState::Ready(runtime) => runtime,
                SlotState::Off => return Ok(()),
                SlotState::Pending => return Err(not_ready()),
            };
            let controller = runtime.controller_for(&self.doctype).await?;
            let mut due: Vec<(i32, StepName)> = controller
                .steps()
                .into_iter()
                .zip(controller.scripts())
                .filter(|(_, script)| script.event == event)
                .map(|(decl, _)| (decl.priority, decl.name))
                .collect();
            if due.is_empty() {
                return Ok(());
            }
            if due.len() > runtime.settings.max_scripts_per_event as usize {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    spot_server_scripts::LIMIT_EXCEEDED,
                    format!(
                        "`{}` has more than {} scripts on `{}`.",
                        self.doctype,
                        runtime.settings.max_scripts_per_event,
                        event.as_str()
                    ),
                ));
            }
            due.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| a.1.as_str().cmp(b.1.as_str()))
            });
            for (_, name) in &due {
                controller.run_step(name, ctx).await?;
            }
            Ok(())
        })
    }
}



pub(super) const STEP_CHECK: &str = "server_script:check_target";
pub(super) const STEP_SAVED: &str = "server_script:reload_saved";
pub(super) const STEP_DELETED: &str = "server_script:reload_deleted";

pub(super) fn text<'a>(doc: &'a Document, field: &str) -> Option<&'a str> {
    match doc.get(field) {
        Value::Text(t) => Some(t.as_str()),
        _ => None,
    }
}

/// Whether the invocation is this module's own write-back (a run record, a disable), which must
/// not bump the cache on top of the explicit bump the writer does.
pub(super) fn is_own(inv: &Invocation) -> bool {
    matches!(&inv.actor, Actor::System { component } if component.as_str() == COMPONENT)
}

/// The controller of the `server_script` DocType.
pub struct ScriptAdmin {
    compile: ServerScriptController,
    slot: Arc<ScriptSlot>,
}

impl ScriptAdmin {
    /// The administration controller over `slot`'s engine and `store` (the compile step records
    /// through it).
    #[must_use]
    pub fn new(slot: Arc<ScriptSlot>, store: Arc<dyn ScriptStore>) -> ScriptAdmin {
        ScriptAdmin {
            compile: ServerScriptController::new(slot.script_engine().clone(), store),
            slot,
        }
    }

    fn bump(&self) {
        if let SlotState::Ready(runtime) = self.slot.state() {
            runtime.invalidate();
        }
    }
}

impl Controller for ScriptAdmin {
    fn doctype(&self) -> &str {
        SERVER_SCRIPT_DOCTYPE
    }

    fn steps(&self) -> Vec<StepDecl> {
        let mut steps = self.compile.steps();
        let named = |name: &str, point: HookPoint, priority: i32| {
            StepName::new(name).ok().map(|name| StepDecl {
                name,
                point,
                priority,
                budget: Duration::from_secs(1),
            })
        };
        steps.extend(named(
            STEP_CHECK,
            HookPoint::of(Phase::Validate, Target::Save),
            0,
        ));
        steps.extend(named(
            STEP_SAVED,
            HookPoint::of(Phase::Committed, Target::Save),
            0,
        ));
        steps.extend(named(
            STEP_DELETED,
            HookPoint::of(Phase::Committed, Target::Op(Operation::Delete)),
            0,
        ));
        steps
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            match step.as_str() {
                STEP_CHECK => {
                    let doctype = text(ctx.doc(), "doctype").unwrap_or_default();
                    if is_scriptable(doctype) {
                        Ok(())
                    } else {
                        Err(AppError::field(
                            FieldPath::field("doctype"),
                            spot_server_scripts::RECORD_INVALID,
                            format!("Scripts cannot be attached to the DocType `{doctype}`."),
                        ))
                    }
                }
                STEP_SAVED | STEP_DELETED => {
                    if !is_own(ctx.invocation()) {
                        self.bump();
                    }
                    Ok(())
                }
                _ => self.compile.run_step(step, ctx).await,
            }
        })
    }
}

