//! The view step: parse each changed `views/*.toml`, resolve it over the live effective
//! metadata and run the save-time validation.
//!
//! Views are layered differences merged over the effective metadata (ADR-068 §7), so a view
//! change never rebuilds metadata: [`spot_views::resolve`] applies the layers and
//! [`spot_views::validate_layer`] enforces the one rule the engine depends on — a hidden
//! `required` field with no default is refused. Nothing is swapped into the runtime here: the
//! resolved view is validated, and the client refetches it after `dev.reloaded`.

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use spot_meta::EffectiveMeta;
use spot_views::{ViewDef, resolve, validate_layer};

use super::{ReloadContext, Step, StepName, StepOutcome, change_file};

/// Where a step reads the site's current effective metadata from.
pub trait DefinitionsSource: Send + Sync {
    /// The effective metadata, or `None` when no metadata is live yet.
    fn effective(&self) -> Option<Arc<EffectiveMeta>>;
}

/// The view reload step.
pub struct ViewStep {
    dir: Utf8PathBuf,
    definitions: Arc<dyn DefinitionsSource>,
}

impl ViewStep {
    /// A step reading changed views from `dir` and validating against `definitions`.
    #[must_use]
    pub fn new(dir: impl Into<Utf8PathBuf>, definitions: Arc<dyn DefinitionsSource>) -> Self {
        Self {
            dir: dir.into(),
            definitions,
        }
    }

    /// The directory the step reads.
    #[must_use]
    pub fn dir(&self) -> &Utf8Path {
        &self.dir
    }

    /// Parses and validates one view file, returning every problem as rendered text.
    ///
    /// Exposed so the CLI's `spot dev check` (and the tests) can validate a view without a
    /// running watcher.
    #[must_use]
    pub fn validate_file(&self, file: &Utf8Path) -> Vec<String> {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) => return vec![format!("{}: {error}", file.as_str())],
        };
        let view = match ViewDef::parse(file.as_str(), &text) {
            Ok(view) => view,
            Err(errors) => {
                return errors.iter().map(|error| format!("{error}")).collect();
            }
        };
        let Some(meta) = self.definitions.effective() else {
            return vec![format!(
                "{}: no metadata is live, so the view cannot be resolved",
                file.as_str()
            )];
        };
        let resolved = resolve(&view, &[], &meta);
        validate_layer(&resolved, &meta)
            .iter()
            .map(|error| format!("{error}"))
            .collect()
    }
}

impl Step for ViewStep {
    fn name(&self) -> StepName {
        StepName::Views
    }

    fn reload(&self, ctx: &ReloadContext<'_>) -> StepOutcome {
        let mut validated = 0_usize;
        let mut errors = Vec::new();
        for change in ctx.changes {
            let file = change_file(&self.dir, change);
            let problems = self.validate_file(&file);
            if problems.is_empty() {
                validated += 1;
            } else {
                errors.extend(problems);
            }
        }
        if errors.is_empty() {
            StepOutcome::ok(format!("{validated} view(s) validated"))
        } else {
            StepOutcome::failed(
                format!(
                    "{} view problem(s); the previous views keep serving",
                    errors.len()
                ),
                errors,
            )
        }
    }
}
