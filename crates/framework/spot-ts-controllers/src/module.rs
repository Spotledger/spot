//! Loading a controller module from an installed app: read the TypeScript, transpile it, compile
//! it on the engine, run its `contract()` and validate the declaration.
//!
//! The TypeScript an app ships is the source of truth; the compiled bytecode the engine keeps is
//! keyed by content hash, so an unchanged module is never transpiled or compiled twice.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as Json;
use spot_app_api::{AppError, AppResult, ErrorClass};
use spot_ts::{Compiled, EngineConfig, HostBridge, HostError, ScriptEngine, ScriptError, ScriptId};
use spot_ts_transpile::{Diagnostic, Options};

use crate::codes;
use crate::contract::{Contract, parse_contract};
use crate::limits::TsLimits;
use crate::problem::Problem;

/// Where a `ts:<path>` module's source comes from: the installed app bundle.
///
/// The installer wires this to the bundle's TypeScript sources. [`MemoryModules`] serves tests and
/// an in-process host.
pub trait ModuleSource: Send + Sync {
    /// The TypeScript source of `path` in `app`.
    ///
    /// # Errors
    ///
    /// `tscontroller.module_missing` when the bundle does not carry the module.
    fn read(&self, app: &str, path: &str) -> AppResult<String>;
}

/// An in-memory [`ModuleSource`]: the modules a host or a test installed.
#[derive(Clone, Debug, Default)]
pub struct MemoryModules {
    modules: BTreeMap<(String, String), String>,
}

impl MemoryModules {
    /// An empty source.
    pub fn new() -> MemoryModules {
        MemoryModules::default()
    }

    /// The same source with `source` under `app`/`path`.
    #[must_use]
    pub fn with(mut self, app: &str, path: &str, source: &str) -> MemoryModules {
        self.modules
            .insert((app.to_owned(), path.to_owned()), source.to_owned());
        self
    }

    /// Adds or replaces one module.
    pub fn insert(&mut self, app: &str, path: &str, source: &str) -> &mut Self {
        self.modules
            .insert((app.to_owned(), path.to_owned()), source.to_owned());
        self
    }
}

impl ModuleSource for MemoryModules {
    fn read(&self, app: &str, path: &str) -> AppResult<String> {
        self.modules
            .get(&(app.to_owned(), path.to_owned()))
            .cloned()
            .ok_or_else(|| {
                AppError::new(
                    ErrorClass::NotFound,
                    codes::MODULE_MISSING,
                    format!(
                        "The installed bundle of `{app}` carries no controller module `{path}`."
                    ),
                )
            })
    }
}

/// A module that loaded and validated: its declaration, its compiled bytecode and the JavaScript
/// the engine runs.
pub struct LoadedModule {
    /// The validated declaration.
    pub contract: Contract,
    /// The compiled bytecode, ready to run.
    pub compiled: Arc<Compiled>,
    /// The transpiled JavaScript, for diagnostics.
    pub js: String,
}

/// Reads, transpiles, compiles and validates the module `path` of `app`.
///
/// # Errors
///
/// Every [`Problem`] at once: a source that does not transpile, a module the engine refuses, a
/// missing `contract()` export, or a declaration that does not match the contract.
pub fn load_module(
    app: &str,
    path: &str,
    source: &str,
    engine: &ScriptEngine,
    limits: TsLimits,
) -> Result<LoadedModule, Vec<Problem>> {
    let options = Options {
        allow_imports: Vec::new(),
        ..Options::default()
    };
    let transpiled =
        spot_ts_transpile::transpile(source, path, &options).map_err(|diagnostics| {
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic_problem(path, diagnostic))
                .collect::<Vec<_>>()
        })?;
    let compiled = engine
        .compile(
            &transpiled.js,
            ScriptId::new(app, path, 0),
            Some(&transpiled.source_map),
        )
        .map_err(|error| vec![engine_problem(path, &error)])?;
    let declaration = run_contract(app, path, &compiled, engine, limits)?;
    let contract = parse_contract(&declaration)?;
    Ok(LoadedModule {
        contract,
        compiled,
        js: transpiled.js,
    })
}

/// Runs the module's `contract()` export and returns its JSON result.
fn run_contract(
    app: &str,
    path: &str,
    compiled: &Arc<Compiled>,
    engine: &ScriptEngine,
    limits: TsLimits,
) -> Result<Json, Vec<Problem>> {
    let bridge: Arc<dyn HostBridge> = Arc::new(NullBridge);
    let result = engine.run_blocking(
        compiled,
        "contract",
        &Json::Null,
        bridge,
        limits.engine_limits(0),
    );
    let _ = app;
    match result {
        Ok(value) => Ok(value),
        Err(ScriptError::MissingEntry(entry)) => Err(vec![Problem::new(
            codes::INVALID_MODULE,
            "contract",
            format!("the module exports no `{entry}()` function declaring its contract"),
        )]),
        Err(error) => Err(vec![engine_problem(path, &error)]),
    }
}

/// A [`Problem`] for one transpiler diagnostic, at its position.
fn diagnostic_problem(path: &str, diagnostic: &Diagnostic) -> Problem {
    Problem::new(codes::COMPILE_FAILED, path, diagnostic.message.clone()).at(
        spot_ts::SourcePosition {
            source: path.to_owned(),
            line: diagnostic.line,
            column: diagnostic.column,
        },
    )
}

/// A [`Problem`] for an engine failure, with its position when the engine knows one.
fn engine_problem(path: &str, error: &ScriptError) -> Problem {
    let code = match error {
        ScriptError::Compile(_) => codes::COMPILE_FAILED,
        _ => codes::INVALID_MODULE,
    };
    let mut problem = Problem::new(code, path, error.to_string());
    if let ScriptError::Thrown {
        position: Some(position),
        ..
    } = error
    {
        problem = problem.at(position.clone());
    }
    problem
}

/// A bridge that refuses every host call: a module's `contract()` describes, it does not act.
struct NullBridge;

impl HostBridge for NullBridge {
    fn call(&self, name: &str, _args: Json) -> Result<Json, HostError> {
        Err(HostError::new(
            "unsupported",
            &format!("`{name}` is not available while a module describes its contract"),
        ))
    }

    fn now_millis(&self) -> i64 {
        0
    }

    fn random_seed(&self) -> u64 {
        0
    }
}

/// A module compiled and run for a test, without an app bundle.
///
/// # Errors
///
/// Whatever [`load_module`] returns.
pub fn load_in_memory(
    app: &str,
    path: &str,
    source: &str,
    limits: TsLimits,
) -> Result<LoadedModule, Vec<Problem>> {
    let engine = ScriptEngine::new(EngineConfig {
        surface: crate::bridge::ts_host_surface(),
        ..EngineConfig::default()
    });
    load_module(app, path, source, &engine, limits)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const GOOD: &str = r#"
const controller = {
  doctype: "invoice",
  steps: {},
  methods: {
    approve: {
      right: "write",
      args: { amount: "decimal" },
      result: { ok: "bool" },
      handler({ args }) { return { ok: args.amount !== "0" }; },
    },
  },
};
export default controller;
export function contract() {
  return {
    doctype: controller.doctype,
    steps: {},
    methods: {
      approve: { right: "write", args: { amount: "decimal" }, result: { ok: "bool" } },
    },
  };
}
export function run_step() { return null; }
export function run_method({ method, doc, ctx, args }) {
  return controller.methods[method].handler({ doc, ctx, args });
}
"#;

    #[test]
    fn a_good_module_loads_and_declares_its_methods() {
        let loaded = load_in_memory(
            "erp_selling",
            "controllers/invoice.ts",
            GOOD,
            TsLimits::default(),
        )
        .unwrap();
        assert_eq!(loaded.contract.doctype, "invoice");
        assert_eq!(loaded.contract.methods.len(), 1);
    }

    #[test]
    fn a_missing_contract_export_is_a_problem() {
        let problems = load_in_memory("app", "m.ts", "export default {};", TsLimits::default())
            .err()
            .expect("problems");
        assert!(problems.iter().any(|p| p.message.contains("contract")));
    }

    #[test]
    fn a_source_that_does_not_transpile_carries_positions() {
        let problems = load_in_memory(
            "app",
            "m.ts",
            "export function contract( {",
            TsLimits::default(),
        )
        .err()
        .expect("problems");
        assert!(problems.iter().any(|p| p.position.is_some()));
    }
}
