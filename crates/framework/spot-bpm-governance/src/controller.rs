//! The document engine hook: a `before_create` step that runs the guard for one record type.
//!
//! The step sits at the engine's pre-create hook, so **every** channel that creates a record —
//! the native API, an import, a server script, a process step — runs it, because they all go
//! through the record's own create lifecycle (ADR-49, ADR-080 section 1). Nothing is enforced in
//! the UI.

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::context::{Actor, Invocation};
use spot_app_api::lifecycle::{HookPoint, Operation, Phase, Target};
use spot_app_api::{AppResult, BoxFuture, Controller, Document, HookContext, StepDecl, StepName};

use crate::decision::{GovernanceDecision, RefusalReason};
use crate::error::GovernanceError;
use crate::guard::{CreateAttempt, GovernanceGuard};
use crate::permission::PrincipalSource;

/// The document field the outside-process reason is read from, when the caller supplies it that
/// way. A host may instead pass a reason through its own [`CreateAttempt`].
pub const REASON_FIELD: &str = "outside_process_reason";

/// The step name, `<app>:<step>`.
pub const STEP_NAME: &str = "bpm:governance_create";

/// The `before_create` step for one governed record type.
///
/// The host registers one controller per record type that may be governed; a record type whose
/// mode is `free` passes through with no effect.
pub struct GovernanceController {
    doctype: String,
    guard: Arc<GovernanceGuard>,
    principals: Arc<dyn PrincipalSource>,
    reason_field: String,
}

impl GovernanceController {
    /// A controller for `doctype`, deciding through `guard` and resolving principals through
    /// `principals`.
    #[must_use]
    pub fn new(
        doctype: &str,
        guard: Arc<GovernanceGuard>,
        principals: Arc<dyn PrincipalSource>,
    ) -> Self {
        Self {
            doctype: doctype.to_owned(),
            guard,
            principals,
            reason_field: REASON_FIELD.to_owned(),
        }
    }

    /// Reads the reason from a different document field.
    #[must_use]
    pub fn with_reason_field(mut self, field: &str) -> Self {
        self.reason_field = field.to_owned();
        self
    }

    /// The company a record is created in: its `company` field when it has one, else the site.
    fn company_of(doc: &Document, inv: &Invocation) -> String {
        doc.text("company")
            .ok()
            .flatten()
            .filter(|text| !text.trim().is_empty())
            .map_or_else(|| inv.site.as_str().to_owned(), str::to_owned)
    }

    /// Runs the guard for the document the step runs for.
    fn decide(&self, ctx: &mut dyn HookContext) -> Result<(), GovernanceError> {
        let doc = ctx.doc();
        let inv = ctx.invocation();
        let company = Self::company_of(doc, inv);
        let actor: Actor = inv.actor.clone();
        let principal = self.principals.principal(&actor);
        let reason = doc.text(&self.reason_field).ok().flatten();
        let command_id = inv.command_id.as_ref().map(|c| c.as_str().to_owned());
        // The command id is an idempotency key the caller chooses, so it is only a request
        // context when the authority authenticates it for this actor and the current attempt.
        let request = command_id
            .as_deref()
            .and_then(|key| self.guard.authenticate(key, &actor));
        let attempt = CreateAttempt {
            company: &company,
            doctype: self.doctype.as_str(),
            document_id: doc.id(),
            actor: &actor,
            principal: &principal,
            reason,
            request: request.as_ref(),
            command_id: command_id.as_deref(),
            now: inv.now,
        };
        match self.guard.before_create(&attempt)? {
            GovernanceDecision::Allow { .. } | GovernanceDecision::Defer { .. } => Ok(()),
            GovernanceDecision::Refuse { reason } => Err(self.refusal_error(reason)),
        }
    }

    fn refusal_error(&self, reason: RefusalReason) -> GovernanceError {
        match reason {
            RefusalReason::ReasonRequired { min_chars } => GovernanceError::ReasonRequired {
                doctype: self.doctype.clone(),
                min_chars,
            },
            RefusalReason::NoPermission | RefusalReason::NotInCase => {
                GovernanceError::OutsideProcessDenied {
                    doctype: self.doctype.clone(),
                }
            }
        }
    }
}

impl Controller for GovernanceController {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        let Ok(name) = StepName::new(STEP_NAME) else {
            return Vec::new();
        };
        vec![StepDecl {
            name,
            point: HookPoint::of(Phase::Before, Target::Op(Operation::Create)),
            // Platform steps use multiples of 100 (ADR-51); governance runs first, before any
            // other `before_create` step can change the document.
            priority: 100,
            budget: Duration::from_secs(5),
        }]
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if step.as_str() != STEP_NAME {
                return Ok(());
            }
            if ctx.point() != &HookPoint::of(Phase::Before, Target::Op(Operation::Create)) {
                return Ok(());
            }
            self.decide(ctx).map_err(|error| error.to_app_error())
        })
    }
}
