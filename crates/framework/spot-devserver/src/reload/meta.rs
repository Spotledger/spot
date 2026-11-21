//! The metadata step: load `meta/*.toml`, build a whole new snapshot, swap it in only if it
//! validated.
//!
//! This is the strongest guarantee in the pipeline and it is not re-implemented here:
//! [`LiveMeta::apply`] with [`MetaChange::SetDefinitions`] builds the metadata **and** its
//! compiled plans before it moves the pointer, so a change that does not build leaves the
//! previous snapshot serving and reports every error at once. The step only renders those
//! errors for the developer.

use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use spot_core::SiteId;
use spot_meta::{BuildOptions, Definition};
use spot_meta_live::{LiveError, LiveMeta, MetaChange, Version};

use super::views::DefinitionsSource;
use super::{ReloadContext, Step, StepName, StepOutcome};
use crate::error::{DevError, DevResult};
use crate::paths;

/// Loads every `*.toml` under `dir` as a [`Definition`], deterministically (sorted by path).
///
/// A missing directory or a single bad file is an error carrying every problem found, so the
/// caller never applies a partial definition set.
///
/// # Errors
///
/// [`DevError::Metadata`] with one rendered problem per bad or missing file.
pub fn load_definitions(dir: &Utf8Path) -> DevResult<Vec<Definition>> {
    let files = paths::walk_files(dir, "toml")?;
    if files.is_empty() {
        return Err(DevError::Metadata {
            errors: vec![format!("no metadata TOML files found under `{dir}`")],
        });
    }
    let mut definitions = Vec::with_capacity(files.len());
    let mut errors = Vec::new();
    for file in files {
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) => {
                errors.push(format!("{}: {error}", file.as_str()));
                continue;
            }
        };
        match Definition::parse(file.as_str(), &text) {
            Ok(definition) => definitions.push(definition),
            Err(error) => errors.push(format!("{}: {error}", file.as_str())),
        }
    }
    if errors.is_empty() {
        Ok(definitions)
    } else {
        Err(DevError::Metadata { errors })
    }
}

/// Renders every problem in a [`LiveError`], code first, so the developer sees the fix.
#[must_use]
pub fn live_errors(error: &LiveError) -> Vec<String> {
    match error {
        LiveError::Build {
            version,
            meta,
            plans,
        } => {
            let mut out = vec![format!(
                "{}: metadata version {version} does not build",
                error.code()
            )];
            out.extend(meta.iter().map(|problem| format!("  {problem}")));
            out.extend(plans.iter().map(|problem| format!("  {problem}")));
            out
        }
        other => vec![format!("{}: {other}", other.code())],
    }
}

/// The metadata reload step, owning the site's live snapshot.
pub struct MetaStep {
    site: SiteId,
    dir: Utf8PathBuf,
    options: BuildOptions,
    live: Arc<LiveMeta>,
}

impl MetaStep {
    /// Opens `dir` as the site's authored metadata and builds the first snapshot.
    ///
    /// # Errors
    ///
    /// [`DevError::Metadata`] when the directory has no usable definitions or the first build
    /// fails.
    pub fn open(site: SiteId, dir: impl Into<Utf8PathBuf>) -> DevResult<Self> {
        Self::open_with(site, dir, BuildOptions::new())
    }

    /// As [`MetaStep::open`] with explicit [`BuildOptions`] (known seams, derivations).
    ///
    /// # Errors
    ///
    /// [`DevError::Metadata`] when the directory has no usable definitions or the first build
    /// fails.
    pub fn open_with(
        site: SiteId,
        dir: impl Into<Utf8PathBuf>,
        options: BuildOptions,
    ) -> DevResult<Self> {
        let dir = dir.into();
        let definitions = load_definitions(&dir)?;
        let live = LiveMeta::with(site.clone(), definitions, options.clone(), Version::ZERO)
            .map_err(|error| DevError::Metadata {
                errors: live_errors(&error),
            })?;
        Ok(Self {
            site,
            dir,
            options,
            live: Arc::new(live),
        })
    }

    /// The site these snapshots belong to.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The directory the step loads.
    #[must_use]
    pub fn dir(&self) -> &Utf8Path {
        &self.dir
    }

    /// The build options every later rebuild runs with.
    #[must_use]
    pub fn options(&self) -> &BuildOptions {
        &self.options
    }

    /// The live snapshot holder — the handle the runtime reads through.
    #[must_use]
    pub fn live(&self) -> &Arc<LiveMeta> {
        &self.live
    }

    /// The current metadata version.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.live.current().version().get()
    }
}

impl Step for MetaStep {
    fn name(&self) -> StepName {
        StepName::Metadata
    }

    fn reload(&self, _ctx: &ReloadContext<'_>) -> StepOutcome {
        // The whole tree is reloaded, not only the changed file: metadata references (links,
        // child tables) cross files, so a partial rebuild could reject a valid change.
        let definitions = match load_definitions(&self.dir) {
            Ok(definitions) => definitions,
            Err(error) => {
                let errors = vec![error.to_string()];
                return StepOutcome::failed(error.to_string(), errors);
            }
        };
        match self.live.apply(MetaChange::SetDefinitions { definitions }) {
            Ok(version) => StepOutcome::ok(format!("metadata live at version {}", version.get()))
                .with_version(version.get()),
            Err(error) => {
                let errors = live_errors(&error);
                StepOutcome::failed(
                    format!(
                        "metadata unchanged; the change did not build ({})",
                        error.code()
                    ),
                    errors,
                )
            }
        }
    }
}

/// The current definitions of a [`LiveMeta`], for the view and script steps.
pub struct LiveDefinitions {
    live: Arc<LiveMeta>,
}

impl LiveDefinitions {
    /// Wraps the live holder.
    #[must_use]
    pub fn new(live: Arc<LiveMeta>) -> Self {
        Self { live }
    }
}

impl DefinitionsSource for LiveDefinitions {
    fn effective(&self) -> Option<Arc<spot_meta::EffectiveMeta>> {
        Some(self.live.current().meta().clone())
    }
}

impl std::fmt::Debug for LiveDefinitions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveDefinitions")
            .field("version", &self.live.current().version().get())
            .finish()
    }
}
