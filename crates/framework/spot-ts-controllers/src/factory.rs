//! The TypeScript controller factory: turning a `ts:<path>` reference into a [`Controller`].
//!
//! A host builds one factory per app (the app name is part of every registered step's name) and
//! registers it with [`ControllerRegistry::register_from_manifest_ref`], which records the
//! controller under [`ControllerSource::TypeScript`]. The registry's method-index rules
//! (ambiguity, override) apply to a TypeScript method exactly as to a native one.
//!
//! ```no_run
//! use std::sync::Arc;
//! use spot_controllers::ControllerSource;
//! use spot_ts_controllers::{MemoryModules, TsControllerFactory};
//! use spot_ts::{EngineConfig, ScriptEngine};
//! # use spot_controllers::ControllerRegistry;
//! # use spot_manifest::AppManifest;
//! # fn build(manifest: &AppManifest) {
//! let engine = ScriptEngine::new(EngineConfig::default());
//! let modules = MemoryModules::new()
//!     .with("erp_selling", "controllers/invoice.ts", "export function contract() { return { doctype: \"invoice\" }; }");
//! let factory = TsControllerFactory::new("erp_selling", engine, Arc::new(modules));
//! let mut registry = ControllerRegistry::new();
//! let reference = spot_controllers::ControllerRef::parse("ts:controllers/invoice.ts").unwrap();
//! registry
//!     .register_from_manifest_ref(manifest, &reference, &factory)
//!     .expect("the module loads");
//! assert_eq!(registry.controllers_for("invoice")[0].source, ControllerSource::TypeScript);
//! # }
//! ```

use std::sync::Arc;

use spot_app_api::{AppError, AppResult, Controller, ErrorClass};
use spot_controllers::{ControllerFactory, ControllerRef, ControllerSource, NativeControllers};
use spot_server_scripts::FieldView;
use spot_ts::ScriptEngine;

use crate::codes;
use crate::controller::TsController;
use crate::limits::TsLimits;
use crate::module::{ModuleSource, load_module};
use crate::problem::Problem;

/// Turns `ts:<path>` references into [`TsController`]s loaded from an app's installed bundle.
pub struct TsControllerFactory {
    app: String,
    engine: ScriptEngine,
    modules: Arc<dyn ModuleSource>,
    limits: TsLimits,
    view: Option<Arc<dyn FieldView>>,
    fallback: Arc<dyn ControllerFactory>,
}

impl TsControllerFactory {
    /// A factory for `app`'s modules, running on `engine`.
    pub fn new(
        app: &str,
        engine: ScriptEngine,
        modules: Arc<dyn ModuleSource>,
    ) -> TsControllerFactory {
        TsControllerFactory {
            app: app.to_owned(),
            engine,
            modules,
            limits: TsLimits::default(),
            view: None,
            fallback: Arc::new(NativeControllers::new()),
        }
    }

    /// The same factory with these limits.
    #[must_use]
    pub fn with_limits(mut self, limits: TsLimits) -> TsControllerFactory {
        self.limits = limits;
        self
    }

    /// The same factory giving handlers the host's document view (field access).
    #[must_use]
    pub fn with_view(mut self, view: Arc<dyn FieldView>) -> TsControllerFactory {
        self.view = Some(view);
        self
    }

    /// The same factory delegating `native:`/`wasm:` references to `fallback`.
    #[must_use]
    pub fn with_fallback(mut self, fallback: Arc<dyn ControllerFactory>) -> TsControllerFactory {
        self.fallback = fallback;
        self
    }

    /// The app whose bundle the factory reads.
    pub fn app(&self) -> &str {
        &self.app
    }
}

impl ControllerFactory for TsControllerFactory {
    fn create(&self, reference: &ControllerRef) -> AppResult<Arc<dyn Controller>> {
        if reference.source() != ControllerSource::TypeScript {
            return self.fallback.create(reference);
        }
        let path = reference.value();
        let source = self.modules.read(&self.app, path)?;
        let loaded = load_module(&self.app, path, &source, &self.engine, self.limits)
            .map_err(module_error)?;
        let mut controller =
            TsController::from_parts(&self.app, path, loaded, self.engine.clone(), self.limits);
        if let Some(view) = &self.view {
            controller = controller.with_view(Arc::clone(view));
        }
        Ok(Arc::new(controller))
    }
}

/// The app error a module that does not match the contract surfaces as: every problem, in the
/// support detail.
pub fn module_error(problems: Vec<Problem>) -> AppError {
    let rendered = problems
        .iter()
        .map(Problem::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    let code = if problems
        .iter()
        .all(|problem| problem.code == codes::COMPILE_FAILED)
    {
        codes::COMPILE_FAILED
    } else {
        codes::INVALID_MODULE
    };
    AppError::new(
        ErrorClass::Validation,
        code,
        "The TypeScript controller module is not usable.",
    )
    .detail("problems", rendered)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::module::MemoryModules;
    use spot_ts::{EngineConfig, ScriptEngine};

    const MODULE: &str = r#"
export function contract() {
  return {
    doctype: "invoice",
    steps: { derive_total: { point: "before_save" } },
    methods: { approve: { right: "write", args: { amount: "decimal" } } },
  };
}
export function run_step() { return null; }
export function run_method() { return { ok: true }; }
"#;

    #[test]
    fn a_ts_reference_loads_and_registers() {
        let engine = ScriptEngine::new(EngineConfig::default());
        let modules = MemoryModules::new().with("erp_selling", "controllers/invoice.ts", MODULE);
        let factory = TsControllerFactory::new("erp_selling", engine, Arc::new(modules));
        let reference = ControllerRef::parse("ts:controllers/invoice.ts").unwrap();
        let controller = factory.create(&reference).unwrap();
        assert_eq!(controller.doctype(), "invoice");
        assert_eq!(controller.steps().len(), 1);
        assert_eq!(controller.methods().len(), 1);
    }

    #[test]
    fn a_missing_module_is_module_missing() {
        let engine = ScriptEngine::new(EngineConfig::default());
        let factory = TsControllerFactory::new("app", engine, Arc::new(MemoryModules::new()));
        let reference = ControllerRef::parse("ts:controllers/gone.ts").unwrap();
        let error = factory.create(&reference).err().expect("an error");
        assert_eq!(error.code, codes::MODULE_MISSING);
    }

    #[test]
    fn a_native_reference_delegates_to_the_fallback() {
        let engine = ScriptEngine::new(EngineConfig::default());
        let factory = TsControllerFactory::new("app", engine, Arc::new(MemoryModules::new()));
        let reference = ControllerRef::parse("native:thing").unwrap();
        let error = factory.create(&reference).err().expect("an error");
        assert_eq!(error.code, spot_controllers::UNKNOWN_NATIVE);
    }
}
