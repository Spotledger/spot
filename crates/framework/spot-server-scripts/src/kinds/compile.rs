//! Compile on save for the two new DocTypes: the same diagnostics as `Server Script`.

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::prelude::{Name, Value};
use spot_app_api::{
    AppResult, BoxFuture, Controller, HookContext, HookPoint, Phase, StepDecl, StepName, Target,
};
use spot_ts::ScriptEngine;

use super::ScriptKind;
use super::record::{ApiScript, ScheduledScript, text};
use super::store::KindStore;
use crate::compile::{compile_with_id, refusal};
use crate::limits::ScriptLimits;
use crate::record::{FIELD_COMPILED_HASH, FIELD_NAME, FIELD_SCRIPT};

/// The `api_script` or `scheduled_script` DocType's own controller. Its `before_save` step
/// validates the record, transpiles and compiles the source, reports every diagnostic with its
/// line and column as a field error on the source field, refuses a source that does not compile
/// or does not export its entry point (`on_call` or `on_run`), and stores the compiled hash.
pub struct KindCompileController {
    kind: ScriptKind,
    engine: ScriptEngine,
    store: Arc<dyn KindStore>,
    limits: ScriptLimits,
}

/// The controller of the `api_script` DocType.
pub type ApiScriptController = KindCompileController;
/// The controller of the `scheduled_script` DocType.
pub type ScheduledScriptController = KindCompileController;

impl KindCompileController {
    /// The compile controller of `kind`.
    pub fn new(
        kind: ScriptKind,
        engine: ScriptEngine,
        store: Arc<dyn KindStore>,
    ) -> KindCompileController {
        KindCompileController {
            kind,
            engine,
            store,
            limits: ScriptLimits::default(),
        }
    }

    /// The `api_script` DocType's controller.
    pub fn api(engine: ScriptEngine, store: Arc<dyn KindStore>) -> KindCompileController {
        KindCompileController::new(ScriptKind::Api, engine, store)
    }

    /// The `scheduled_script` DocType's controller.
    pub fn scheduled(engine: ScriptEngine, store: Arc<dyn KindStore>) -> KindCompileController {
        KindCompileController::new(ScriptKind::Scheduled, engine, store)
    }

    /// The same controller validating timeouts against `limits`.
    #[must_use]
    pub fn with_limits(mut self, limits: ScriptLimits) -> KindCompileController {
        self.limits = limits;
        self
    }

    /// The step the compile-on-save hook runs as: `<doctype>:compile`.
    pub fn step_name(kind: ScriptKind) -> AppResult<StepName> {
        StepName::new(&format!("{}:compile", kind.doctype()))
    }
}

impl Controller for KindCompileController {
    fn doctype(&self) -> &str {
        self.kind.doctype()
    }

    fn steps(&self) -> Vec<StepDecl> {
        match Self::step_name(self.kind) {
            Ok(name) => vec![StepDecl {
                name,
                point: HookPoint::of(Phase::Before, Target::Save),
                priority: -1_000,
                budget: Duration::from_secs(5),
            }],
            Err(_) => Vec::new(),
        }
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let doc = ctx.doc().clone();
            // The record must be well formed before it is worth compiling.
            match self.kind {
                ScriptKind::Api => {
                    ApiScript::from_document_with(&doc, &self.limits)?;
                }
                ScriptKind::Scheduled => {
                    ScheduledScript::from_document_with(&doc, &self.limits)?;
                }
            }
            let name = text(&doc, FIELD_NAME).unwrap_or_default().to_owned();
            let source = text(&doc, FIELD_SCRIPT).unwrap_or_default().to_owned();
            let outcome = compile_with_id(
                &self.engine,
                self.kind.doctype(),
                &name,
                &source,
                Some(self.kind.entry()),
            );
            self.store
                .record_compile(self.kind, &name, outcome.clone())
                .await?;
            if let Some(error) = refusal(&outcome) {
                return Err(error);
            }
            let hash = outcome.hash.clone().unwrap_or_default();
            let doc = ctx.doc_mut()?;
            if let Ok(field) = Name::new(FIELD_COMPILED_HASH) {
                doc.set(field, Value::Text(hash));
            }
            Ok(())
        })
    }
}
