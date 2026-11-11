//! The sample controller (WP-DX-05.T1): the Time Entry hours check and roll-up,
//! and the Task owner derivation, written only against `spot_app_api` (gate G4)
//! with the ADR-51 phase rules respected.
//!
//! # The phases
//!
//! - `validate_save` (phase 2): refuses hours outside 0.25–24 with a field error
//!   on `hours` (`sample.hours_out_of_range`). Phase 2 reads and raises, never
//!   writes.
//! - `after_post` / `after_cancel` (phase 3): the roll-up. The Time Entry's own
//!   document is already written (the write stage precedes `after_*`), so the
//!   step loads the linked Task **through the phase-3 repository**, adjusts
//!   `logged_hours`, and writes it with `Repository::update` — a nested write
//!   that runs the Task's own lifecycle inside the caller's transaction (one
//!   transaction: the roll-up commits or rolls back with the Time Entry). Phase 3
//!   may write *other* documents, never its own (that is `doc_mut`, phase 1).
//! - `before_save` on Task (phase 1): fills the Task's `owner` from the linked
//!   Project's owner when the user left it unset, with an information message
//!   (`sample.owner_from_project`). Phase 1 may change its own document; the
//!   step only ever fills a null, never overwrites.
//!
//! Both controllers are steps-only (ADR-16): no whitelisted methods. Errors are
//! typed (`AppError`).

use std::time::Duration;

use spot_app_api::prelude::{Decimal, Name, RecordId, Value};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, ErrorClass, FieldPath, HookContext, HookPoint,
    Message, Operation, Phase, StepDecl, StepName, Target,
};

use crate::catalogue::{HOURS_OUT_OF_RANGE, OWNER_FROM_PROJECT};
use crate::constants::{
    HOURS_BOUNDS, PROJECT_OWNER, TASK_LOGGED_HOURS, TASK_PROJECT, TASK_PROJECT_OWNER,
    TIME_ENTRY_HOURS, TIME_ENTRY_TASK,
};

/// Lower runs first: the platform steps use multiples of 100, the sample sits
/// between them.
const PRIORITY: i32 = 50;

/// The default step budget in milliseconds (the manifest may override it).
const BUDGET_MS: u64 = 200;

/// The step names, `<app>:<step>`, as the manifest declares them.
pub const STEP_CHECK_HOURS: &str = "sample:check_hours";

/// The after-post roll-up: adds the posted entry's hours to the Task.
pub const STEP_ROLL_UP_ADD: &str = "sample:roll_up_hours_add";

/// The after-cancel roll-up: subtracts the cancelled entry's hours back off.
pub const STEP_ROLL_UP_SUBTRACT: &str = "sample:roll_up_hours_subtract";

/// The before-save owner derivation on Task.
pub const STEP_DERIVE_OWNER: &str = "sample:derive_project_owner";

/// An internal error: the compiled-in constants the steps read are broken.
fn constant_error(name: &str, why: String) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "sample.invalid_name",
        format!("the constant {name} is not a storage name: {why}"),
    )
}

/// `logged_hours` set to `delta` on the Task, through the nested-write rule.
async fn adjust_logged_hours(
    ctx: &mut dyn HookContext,
    task_id: RecordId,
    delta: Decimal,
) -> AppResult<()> {
    // Load through the phase-3 repository: reads are allowed in every phase and
    // the load sees the transaction's own state.
    let task = ctx.load("task", task_id).await?;
    let version = task.version().ok_or_else(|| {
        AppError::new(
            ErrorClass::Internal,
            "sample.roll_up_no_version",
            "The linked Task has no version to update against.",
        )
    })?;
    let current = task.decimal(TASK_LOGGED_HOURS)?;
    let next = current.unwrap_or(Decimal::ZERO) + delta;
    let field = Name::new(TASK_LOGGED_HOURS)
        .map_err(|e| constant_error(TASK_LOGGED_HOURS, e.to_string()))?;
    let mut patch = spot_app_api::DocumentPatch::default();
    patch.fields.insert(field, Value::Decimal(next));
    // Through the repository, not `doc_mut`: phase 3 writes *other* documents
    // with their own lifecycle, in the caller's transaction.
    ctx.update("task", task_id, patch, version).await?;
    Ok(())
}

/// The exact hours of a Time Entry document, or `None` when unset.
fn hours_of(doc: &spot_app_api::Document) -> AppResult<Option<Decimal>> {
    doc.decimal(TIME_ENTRY_HOURS)
}

/// The hours field path, for the field errors.
fn hours_path() -> FieldPath {
    FieldPath::field(TIME_ENTRY_HOURS)
}

/// One step declaration at one point, with the sample's priority and budget.
fn decl(name: &str, point: HookPoint) -> StepDecl {
    StepDecl {
        name: StepName::new(name)
            .unwrap_or_else(|e| panic!("the sample step name {name} is well-formed: {e}")),
        point,
        priority: PRIORITY,
        budget: Duration::from_millis(BUDGET_MS),
    }
}

impl Controller for TimeEntryController {
    fn doctype(&self) -> &str {
        "time_entry"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![
            decl(
                STEP_CHECK_HOURS,
                HookPoint::of(Phase::Validate, Target::Save),
            ),
            decl(
                STEP_ROLL_UP_ADD,
                HookPoint::of(Phase::After, Target::Op(Operation::Post)),
            ),
            decl(
                STEP_ROLL_UP_SUBTRACT,
                HookPoint::of(Phase::After, Target::Op(Operation::Cancel)),
            ),
        ]
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let direction = match step.as_str() {
                STEP_CHECK_HOURS => return check_hours(ctx),
                STEP_ROLL_UP_ADD => Decimal::from(1),
                STEP_ROLL_UP_SUBTRACT => Decimal::from(-1),
                other => {
                    return Err(AppError::new(
                        ErrorClass::Internal,
                        "platform.unknown_step",
                        format!("the sample Time Entry controller holds no step {other}"),
                    ));
                }
            };
            let hours = hours_of(ctx.doc())?.unwrap_or(Decimal::ZERO);
            let Some(task_id) = ctx.doc().link(TIME_ENTRY_TASK)? else {
                // The host refuses a null `task` (required) in phase 2; reaching
                // phase 3 without one can only be a hostless probe.
                return Ok(());
            };
            adjust_logged_hours(ctx, task_id, hours * direction).await
        })
    }
}

/// The hours check and the roll-up, one controller on the `time_entry` DocType.
///
/// The steps are the ones `apps/sample/app.toml` declares: `validate_save` for the
/// range, `after_post` and `after_cancel` for the roll-up.
#[derive(Debug, Clone, Copy, Default)]
pub struct TimeEntryController;

/// Phase 2: the hours range. Refusals carry the field path `hours`.
fn check_hours(ctx: &mut dyn HookContext) -> AppResult<()> {
    let (low, high) = (HOURS_BOUNDS.0, HOURS_BOUNDS.1);
    let parse = |s: &str| {
        s.parse::<Decimal>()
            .map_err(|e| constant_error("HOURS_BOUNDS", e.to_string()))
    };
    let low = parse(low)?;
    let high = parse(high)?;
    match hours_of(ctx.doc())? {
        // Missing values are the host's `document.required` refusal, not this
        // code's.
        None => Ok(()),
        Some(h) if h >= low && h <= high => Ok(()),
        Some(_) => Err(AppError::field(
            hours_path(),
            HOURS_OUT_OF_RANGE,
            format!("Hours must be between {low} and {high}."),
        )
        .explained(
            "A Time Entry books a slice of a working day: below a quarter hour \
             the entry carries no work, above a whole day it should be two entries.",
        )),
    }
}

/// The Task owner derivation: `before_save` fills a null `owner` from the linked
/// Project's owner, proving a phase-1 step that changes its own document and
/// reports a message.
#[derive(Debug, Clone, Copy, Default)]
pub struct TaskController;

impl Controller for TaskController {
    fn doctype(&self) -> &str {
        "task"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![decl(
            STEP_DERIVE_OWNER,
            HookPoint::of(Phase::Before, Target::Save),
        )]
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if step.as_str() != STEP_DERIVE_OWNER {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "platform.unknown_step",
                    format!("the sample Task controller holds no step {}", step.as_str()),
                ));
            }
            // The user set a project owner on the Task: nothing to derive, and
            // the step never overwrites a value it did not put there.
            if matches!(ctx.doc().get(TASK_PROJECT_OWNER), Value::Id(_)) {
                return Ok(());
            }
            let Some(project_id) = ctx.doc().link(TASK_PROJECT)? else {
                // The host refuses a null `project` (required) in phase 2.
                return Ok(());
            };
            let project = match ctx.load("project", project_id).await {
                Ok(project) => project,
                // A user who may not read the project (a role without `project` read) leaves the owner empty.
                Err(e)
                    if matches!(e.class, ErrorClass::PermissionDenied | ErrorClass::NotFound) =>
                {
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            let Some(owner) = project.link(PROJECT_OWNER)? else {
                return Ok(());
            };
            let name = Name::new(TASK_PROJECT_OWNER)
                .map_err(|e| constant_error(TASK_PROJECT_OWNER, e.to_string()))?;
            // Phase 1: the step's own document, through `doc_mut`.
            ctx.doc_mut()?.set(name, Value::Id(owner));
            ctx.message(Message::info(
                OWNER_FROM_PROJECT,
                "The owner was filled from the project's owner.",
            ));
            Ok(())
        })
    }
}

/// The methods of both controllers: none (ADR-16 whitelist is per declaration).
#[must_use]
pub fn method_decls() -> Vec<spot_app_api::MethodDecl> {
    Vec::new()
}
