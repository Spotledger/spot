//! The controller every DocType gets for free.
//!
//! A DocType created in code, or added at runtime after the registry was built, always resolves
//! to a controller: [`DefaultController`]. It declares **no steps and no methods** — the
//! framework's defaults, coercion and validation are the engine's job, not a step — so a
//! DocType with no app controller still saves. It adds nothing an app controller then has to
//! undo.

use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, ErrorClass, HookContext, MethodDecl, StepDecl,
    StepName,
};

use crate::codes;

/// A controller for any DocType, from its name alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultController {
    doctype: String,
}

impl DefaultController {
    /// The default controller for `doctype`.
    pub fn new(doctype: impl Into<String>) -> DefaultController {
        DefaultController {
            doctype: doctype.into(),
        }
    }
}

impl Controller for DefaultController {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        Vec::new()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        Vec::new()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            Err(AppError::new(
                ErrorClass::Internal,
                codes::NO_STEP,
                format!(
                    "The default controller for `{}` declares no step `{step}`.",
                    self.doctype
                ),
            ))
        })
    }
}
