//! Compile on save: TypeScript is transpiled and compiled when the record is written.

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::error::FieldPath;
use spot_app_api::prelude::{Name, Value};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, HookContext, HookPoint, Phase, StepDecl, StepName,
    Target,
};
use spot_ts::{ScriptEngine, ScriptId};
use spot_ts_transpile::{Diagnostic, Options};

use crate::SERVER_SCRIPT_DOCTYPE;
use crate::codes;
use crate::record::{FIELD_COMPILED_HASH, FIELD_NAME, FIELD_SCRIPT};
use crate::store::ScriptStore;

/// The outcome of preparing one script: its compiled hash, or why it was refused.
#[derive(Clone, Debug, PartialEq)]
pub struct CompileOutcome {
    /// The compiled source hash, when the script compiled.
    pub hash: Option<String>,
    /// Every transpile diagnostic, with its line and column, when the source did not transpile.
    pub diagnostics: Vec<Diagnostic>,
    /// The engine's refusal, when the source transpiled but the engine would not compile it.
    pub error: Option<String>,
}

impl CompileOutcome {
    /// Whether the script compiled.
    pub fn ok(&self) -> bool {
        self.hash.is_some()
    }

    /// The compiled hash, when the script compiled.
    pub fn hash(&self) -> Option<&str> {
        self.hash.as_deref()
    }

    /// A short reason, for a log line.
    pub fn reason(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        if let Some(first) = self.diagnostics.first() {
            return format!(
                "line {}, column {}: {}",
                first.line, first.column, first.message
            );
        }
        "did not compile".to_owned()
    }
}

/// Transpiles `source` and compiles the result on `engine`, returning its hash or its
/// diagnostics. Imports are refused (the engine has no module loader).
pub fn compile_source(engine: &ScriptEngine, name: &str, source: &str) -> CompileOutcome {
    let options = Options {
        allow_imports: Vec::new(),
        ..Options::default()
    };
    let file = format!("{name}.ts");
    match spot_ts_transpile::transpile(source, &file, &options) {
        Err(diagnostics) => CompileOutcome {
            hash: None,
            diagnostics,
            error: None,
        },
        Ok(transpiled) => {
            let id = ScriptId::new(SERVER_SCRIPT_DOCTYPE, name, 0);
            match engine.compile(&transpiled.js, id, Some(&transpiled.source_map)) {
                Ok(compiled) => CompileOutcome {
                    hash: Some(compiled.source_hash().to_owned()),
                    diagnostics: Vec::new(),
                    error: None,
                },
                Err(error) => CompileOutcome {
                    hash: None,
                    diagnostics: Vec::new(),
                    error: Some(error.to_string()),
                },
            }
        }
    }
}

/// The `Server Script` DocType's own controller: its `before_save` step transpiles the source,
/// reports every diagnostic as a field error on the source field, refuses to save a script that
/// does not compile, and stores the compiled hash.
pub struct ServerScriptController {
    engine: ScriptEngine,
    store: Arc<dyn ScriptStore>,
}

impl ServerScriptController {
    /// A controller compiling through `engine` and recording through `store`.
    pub fn new(engine: ScriptEngine, store: Arc<dyn ScriptStore>) -> ServerScriptController {
        ServerScriptController { engine, store }
    }

    /// The step the compile-on-save hook runs as.
    pub fn step_name() -> AppResult<StepName> {
        StepName::new("server_script:compile")
    }
}

impl Controller for ServerScriptController {
    fn doctype(&self) -> &str {
        SERVER_SCRIPT_DOCTYPE
    }

    fn steps(&self) -> Vec<StepDecl> {
        match Self::step_name() {
            Ok(name) => vec![StepDecl {
                name,
                point: HookPoint::of(Phase::Before, Target::Save),
                // Before every document-event script on the same DocType, so a script is
                // compiled before anything could run it.
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
            let name = text(&doc, FIELD_NAME).unwrap_or_default().to_owned();
            let source = text(&doc, FIELD_SCRIPT).unwrap_or_default().to_owned();
            let outcome = compile_source(&self.engine, &name, &source);
            self.store
                .record_compile(SERVER_SCRIPT_DOCTYPE, &name, outcome.clone())
                .await?;
            if let Some(error) = &outcome.error {
                return Err(AppError::field(
                    FieldPath::field(FIELD_SCRIPT),
                    codes::COMPILE_FAILED,
                    format!("The script did not compile: {error}"),
                ));
            }
            if !outcome.diagnostics.is_empty() {
                let mut error: Option<AppError> = None;
                for diagnostic in &outcome.diagnostics {
                    let message = format!(
                        "line {}, column {}: {}",
                        diagnostic.line, diagnostic.column, diagnostic.message
                    );
                    error = Some(match error {
                        None => AppError::field(
                            FieldPath::field(FIELD_SCRIPT),
                            &diagnostic.code,
                            message,
                        ),
                        Some(existing) => existing.with_field(
                            FieldPath::field(FIELD_SCRIPT),
                            &diagnostic.code,
                            message,
                        ),
                    });
                }
                return Err(error.unwrap_or_else(|| {
                    AppError::new(
                        spot_app_api::ErrorClass::Internal,
                        codes::COMPILE_FAILED,
                        "The script did not compile.",
                    )
                }));
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

fn text<'a>(doc: &'a spot_app_api::Document, field: &str) -> Option<&'a str> {
    match doc.get(field) {
        Value::Text(t) => Some(t.as_str()),
        _ => None,
    }
}
