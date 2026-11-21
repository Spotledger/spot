//! The script step: transpile a changed `ts/*.ts` on save, check it against the current `.d.ts`,
//! and hot-swap the version the engine runs.
//!
//! Type *checking* is not re-implemented as a TypeScript compiler here. `spot-dts` generates the
//! declaration text the script tier sees; the [`TypeChecker`] seam then checks the transpiled
//! script against it. The default [`DeclarationNameChecker`] proves the cheap, high-value half —
//! every name a script imports from a `spot:` virtual module is declared — while the full
//! `tsc` acceptance lives in `spot-dts`'s own suite. A diagnostic from
//! [`spot_ts_transpile`] fails the step and nothing is swapped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use camino::{Utf8Path, Utf8PathBuf};
use spot_dts::Generated;
use spot_meta::Definition;
use spot_ts_transpile::{Diagnostic, Options, Transpiled, transpile};

use super::views::DefinitionsSource;
use super::{ReloadContext, Step, StepName, StepOutcome, change_file};
use crate::error::{DevError, DevResult};

/// Where the script step reads the current `.d.ts` from.
pub trait DtsProvider: Send + Sync {
    /// The current declarations, `Ok(None)` when there is no metadata to generate from, or the
    /// generation errors.
    ///
    /// # Errors
    ///
    /// One rendered problem per DocType `spot-dts` could not declare.
    fn declarations(&self) -> Result<Option<String>, Vec<String>>;
}

/// The declaration provider over the live effective metadata.
pub struct LiveDtsProvider {
    definitions: Arc<dyn DefinitionsSource>,
}

impl LiveDtsProvider {
    /// Wraps the site's definition source.
    #[must_use]
    pub fn new(definitions: Arc<dyn DefinitionsSource>) -> Self {
        Self { definitions }
    }
}

impl DtsProvider for LiveDtsProvider {
    fn declarations(&self) -> Result<Option<String>, Vec<String>> {
        let Some(meta) = self.definitions.effective() else {
            return Ok(None);
        };
        let definitions: &[Definition] = meta.definitions();
        match spot_dts::generate(definitions) {
            Ok(Generated { text, .. }) => Ok(Some(text)),
            Err(errors) => Err(errors.iter().map(|error| format!("{error}")).collect()),
        }
    }
}

/// Where a transpiled script is swapped into the running engine.
pub trait ScriptHost: Send + Sync {
    /// Swaps the engine's version of `script`. The previous version keeps serving on error.
    ///
    /// # Errors
    ///
    /// [`DevError::Script`] when the host refuses the new version.
    fn swap(&self, script: &Utf8Path, transpiled: &Transpiled) -> DevResult<()>;
}

/// A script host that keeps the latest JavaScript per script, for tests and for a
/// no-engine `spot dev check`.
#[derive(Default)]
pub struct InMemoryScriptHost {
    scripts: Mutex<BTreeMap<String, String>>,
}

impl InMemoryScriptHost {
    /// An empty host.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The JavaScript last swapped in for `script_name`.
    #[must_use]
    pub fn get(&self, script_name: &str) -> Option<String> {
        self.lock().get(script_name).cloned()
    }

    /// How many scripts are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no script is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, String>> {
        self.scripts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ScriptHost for InMemoryScriptHost {
    fn swap(&self, script: &Utf8Path, transpiled: &Transpiled) -> DevResult<()> {
        self.lock()
            .insert(script.as_str().to_owned(), transpiled.js.clone());
        Ok(())
    }
}

/// Checks a transpiled script against the declaration text.
pub trait TypeChecker: Send + Sync {
    /// Every problem found, already rendered; empty means the script type-checks by this
    /// checker's rules.
    fn check(&self, script: &Utf8Path, transpiled: &Transpiled, declarations: &str) -> Vec<String>;
}

/// The default checker: every `spot:` import names a declared export.
///
/// It parses the top-level exported names out of the `.d.ts` text and requires each named import
/// of a `spot:*` module to appear among them. An import of a name the declarations do not
/// export is exactly the class of mistake a `.d.ts` is there to catch, without a TypeScript
/// compiler in the save path.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeclarationNameChecker;

impl TypeChecker for DeclarationNameChecker {
    fn check(&self, script: &Utf8Path, transpiled: &Transpiled, declarations: &str) -> Vec<String> {
        let declared = exported_names(declarations);
        if declared.is_empty() {
            return Vec::new();
        }
        let mut errors = Vec::new();
        for import in &transpiled.imports {
            if !import.module.starts_with("spot:") {
                continue;
            }
            for named in &import.named {
                if !declared.contains(&named.imported) {
                    errors.push(format!(
                        "{}: `{}` is not declared by the current .d.ts (imported from {})",
                        script.as_str(),
                        named.imported,
                        import.module
                    ));
                }
            }
        }
        errors
    }
}

/// Top-level names the declaration text exports.
fn exported_names(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("export ") else {
            continue;
        };
        let rest = rest.strip_prefix("declare ").unwrap_or(rest);
        let mut parts = rest.split_whitespace();
        let Some(kind) = parts.next() else {
            continue;
        };
        match kind {
            "interface" | "type" | "const" | "let" | "var" | "function" | "class" | "enum"
            | "namespace" | "abstract" => {
                if let Some(name) = parts.next() {
                    names.insert(trim_name(name));
                }
            }
            _ => {}
        }
    }
    names
}

/// The identifier out of a declaration name, without generics or trailing punctuation.
fn trim_name(name: &str) -> String {
    name.trim_end_matches(|character: char| {
        !(character.is_ascii_alphanumeric() || character == '_' || character == '$')
    })
    .to_owned()
}

/// The script reload step.
pub struct ScriptStep {
    dir: Utf8PathBuf,
    dts: Arc<dyn DtsProvider>,
    checker: Arc<dyn TypeChecker>,
    host: Arc<dyn ScriptHost>,
    options: Options,
}

impl ScriptStep {
    /// A step reading scripts from `dir`, checking against `dts` and swapping into `host`.
    #[must_use]
    pub fn new(
        dir: impl Into<Utf8PathBuf>,
        dts: Arc<dyn DtsProvider>,
        checker: Arc<dyn TypeChecker>,
        host: Arc<dyn ScriptHost>,
    ) -> Self {
        Self {
            dir: dir.into(),
            dts,
            checker,
            host,
            options: Options::default(),
        }
    }

    /// Uses `options` for the transpile (import allow-list, size and depth caps).
    #[must_use]
    pub fn with_options(mut self, options: Options) -> Self {
        self.options = options;
        self
    }

    /// The directory the step reads.
    #[must_use]
    pub fn dir(&self) -> &Utf8Path {
        &self.dir
    }

    /// Transpiles and checks one script without swapping it.
    ///
    /// # Errors
    ///
    /// [`DevError::Script`] with every diagnostic and type-check problem.
    pub fn check_file(&self, file: &Utf8Path) -> DevResult<Transpiled> {
        let source = std::fs::read_to_string(file)
            .map_err(|error| DevError::io_message(file, error.to_string()))?;
        let transpiled =
            transpile(&source, file.as_str(), &self.options).map_err(|diagnostics| {
                DevError::Script {
                    errors: render_diagnostics(&diagnostics),
                }
            })?;
        match self.dts.declarations() {
            Ok(Some(declarations)) => {
                let errors = self.checker.check(file, &transpiled, &declarations);
                if errors.is_empty() {
                    Ok(transpiled)
                } else {
                    Err(DevError::Script { errors })
                }
            }
            Ok(None) => Ok(transpiled),
            Err(errors) => Err(DevError::Script { errors }),
        }
    }
}

impl Step for ScriptStep {
    fn name(&self) -> StepName {
        StepName::Scripts
    }

    fn reload(&self, ctx: &ReloadContext<'_>) -> StepOutcome {
        let mut swapped = 0_usize;
        let mut errors = Vec::new();
        for change in ctx.changes {
            let file = change_file(&self.dir, change);
            match self.check_file(&file) {
                Ok(transpiled) => match self.host.swap(&file, &transpiled) {
                    Ok(()) => {
                        swapped += 1;
                    }
                    Err(error) => errors.push(error.to_string()),
                },
                Err(error) => errors.push(error.to_string()),
            }
        }
        if errors.is_empty() {
            StepOutcome::ok(format!("{swapped} script(s) reloaded"))
        } else {
            StepOutcome::failed(
                format!(
                    "{} script problem(s); the previous scripts keep serving",
                    errors.len()
                ),
                errors,
            )
        }
    }
}

/// Renders transpile diagnostics, code and position first.
fn render_diagnostics(diagnostics: &[Diagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .map(|diagnostic| {
            format!(
                "{}:{}:{}: {}",
                diagnostic.code, diagnostic.line, diagnostic.column, diagnostic.message
            )
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn exported_names_reads_the_declaration_shapes() {
        let text = "export interface Widget { id: string }\nexport namespace spot { }\nexport declare const VERSION: string;\n";
        let names = exported_names(text);
        assert!(names.contains("Widget"));
        assert!(names.contains("spot"));
        assert!(names.contains("VERSION"));
    }

    #[test]
    fn checker_flags_an_unknown_spot_import() {
        let source = "import { getDoc } from \"spot:meta\";\nexport const value = getDoc;\n";
        let transpiled = transpile(source, "main.ts", &Options::default()).unwrap();
        let declared = "export function getDoc(): unknown;\n";
        let errors = DeclarationNameChecker.check(Utf8Path::new("main.ts"), &transpiled, declared);
        assert!(errors.is_empty());
        let missing = DeclarationNameChecker.check(
            Utf8Path::new("main.ts"),
            &transpiled,
            "export function somethingElse(): unknown;\n",
        );
        assert_eq!(missing.len(), 1);
    }
}
