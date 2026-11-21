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
    compile_with_id(engine, SERVER_SCRIPT_DOCTYPE, name, source, None)
}

/// [`compile_source`] for any script kind: the engine's cache id is `<namespace>/<name>`, and
/// when `entry` is given the compiled module must export a function of that name (a script
/// without its entry point is refused naming the missing export).
pub(crate) fn compile_with_id(
    engine: &ScriptEngine,
    namespace: &str,
    name: &str,
    source: &str,
    entry: Option<&str>,
) -> CompileOutcome {
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
            if let Some(entry) = entry
                && !exports_entry(&transpiled.js, entry)
            {
                return CompileOutcome {
                    hash: None,
                    diagnostics: Vec::new(),
                    error: Some(format!("the script does not export a function `{entry}`")),
                };
            }
            let id = ScriptId::new(namespace, name, 0);
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

/// Whether the ES module `js` exports a binding called `entry`: `export function entry`,
/// `export async function entry`, `export const entry`, or `export { entry }` (the forms the
/// transpiler leaves in place). A textual check, so it can only be fooled by a string that
/// mimics an export; the engine's own `script.missing_entry` stays the backstop at run time.
pub(crate) fn exports_entry(js: &str, entry: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    for (at, _) in js.match_indices(entry) {
        let before = &js[..at];
        let after = &js[at + entry.len()..];
        if before.chars().next_back().is_some_and(is_ident)
            || after.chars().next().is_some_and(is_ident)
        {
            continue;
        }
        // `export [async] function[*] entry`, `export const|let|var entry`.
        let mut head = before.trim_end();
        loop {
            let mut stripped = false;
            for word in ["function*", "function", "async", "const", "let", "var"] {
                if let Some(rest) = head.strip_suffix(word)
                    && !rest.chars().next_back().is_some_and(is_ident)
                {
                    head = rest.trim_end();
                    stripped = true;
                }
            }
            if !stripped {
                break;
            }
        }
        if let Some(rest) = head.strip_suffix("export")
            && !rest.chars().next_back().is_some_and(is_ident)
        {
            return true;
        }
        // `export { a, entry, b as c }`: the nearest open brace, no close brace in between,
        // and `export` right before it.
        if let Some(open) = before.rfind('{')
            && !before[open..].contains('}')
            && let Some(rest) = before[..open].trim_end().strip_suffix("export")
            && !rest.chars().next_back().is_some_and(is_ident)
        {
            return true;
        }
    }
    false
}

/// The save-time refusal for a failed compile, as field errors on the source field with the
/// line and column of every diagnostic; `None` when the script compiled.
pub(crate) fn refusal(outcome: &CompileOutcome) -> Option<AppError> {
    if let Some(error) = &outcome.error {
        return Some(AppError::field(
            FieldPath::field(FIELD_SCRIPT),
            codes::COMPILE_FAILED,
            format!("The script did not compile: {error}"),
        ));
    }
    let mut error: Option<AppError> = None;
    for diagnostic in &outcome.diagnostics {
        let message = format!(
            "line {}, column {}: {}",
            diagnostic.line, diagnostic.column, diagnostic.message
        );
        error = Some(match error {
            None => AppError::field(FieldPath::field(FIELD_SCRIPT), &diagnostic.code, message),
            Some(existing) => {
                existing.with_field(FieldPath::field(FIELD_SCRIPT), &diagnostic.code, message)
            }
        });
    }
    if error.is_none() && outcome.hash.is_none() {
        return Some(AppError::new(
            spot_app_api::ErrorClass::Internal,
            codes::COMPILE_FAILED,
            "The script did not compile.",
        ));
    }
    error
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

fn text<'a>(doc: &'a spot_app_api::Document, field: &str) -> Option<&'a str> {
    match doc.get(field) {
        Value::Text(t) => Some(t.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_ts::EngineConfig;

    fn engine() -> ScriptEngine {
        ScriptEngine::new(EngineConfig {
            surface: crate::host::host_surface(),
            ..EngineConfig::default()
        })
    }

    #[test]
    fn a_syntax_error_is_refused_with_a_position() {
        let outcome = compile_with_id(
            &engine(),
            "api_script",
            "broken",
            "export function on_call( {",
            Some("on_call"),
        );
        assert!(!outcome.ok());
        assert!(!outcome.diagnostics.is_empty());
        assert!(outcome.diagnostics[0].line >= 1);
        let refusal = refusal(&outcome).expect("refused");
        assert!(refusal.fields[0].message.contains("line "));
    }

    #[test]
    fn a_script_without_its_entry_point_is_refused_naming_the_export() {
        let outcome = compile_with_id(
            &engine(),
            "api_script",
            "no_entry",
            "export function something_else() { return 1; }",
            Some("on_call"),
        );
        assert!(!outcome.ok());
        let refusal = refusal(&outcome).expect("refused");
        assert_eq!(refusal.fields[0].code, codes::COMPILE_FAILED);
        assert!(refusal.fields[0].message.contains("on_call"));
    }

    #[test]
    fn a_script_with_its_entry_point_compiles() {
        let outcome = compile_with_id(
            &engine(),
            "scheduled_script",
            "fine",
            "export async function on_run({ ctx }: { ctx: unknown }): Promise<void> {}",
            Some("on_run"),
        );
        assert!(outcome.ok(), "{}", outcome.reason());
        assert!(refusal(&outcome).is_none());
    }

    #[test]
    fn the_entry_point_check_reads_the_export_forms() {
        for source in [
            "export function on_call() {}",
            "export async function on_call() {}",
            "export const on_call = () => 1;",
            "function helper() {}\nexport { helper, on_call };\nfunction on_call() {}",
        ] {
            assert!(exports_entry(source, "on_call"), "{source}");
        }
        for source in [
            "function on_call() {}",
            "export function on_call_later() {}",
            "export function not_on_call() {}",
            "const x = 'on_call';",
        ] {
            assert!(!exports_entry(source, "on_call"), "{source}");
        }
    }

    #[test]
    fn the_document_event_compile_still_has_no_entry_requirement() {
        let outcome = compile_source(&engine(), "plain", "export function anything() {}");
        assert!(outcome.ok());
    }
}
