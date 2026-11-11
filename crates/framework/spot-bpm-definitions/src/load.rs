//! Rebuilding the engine's [`ProcessRegistry`] from the store.

use spot_bpm_engine::{ProcessDefinition, ProcessRegistry};
use spot_bpm_model::{VersionStatus, from_text};

use crate::error::Result;
use crate::store::DefinitionStore;
use crate::types::{MAX_PAGE, VersionState};

/// Why one stored version did not load. A stored version that no longer compiles is a startup
/// finding, never skipped silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadFailureKind {
    /// The stored text no longer parses.
    TextInvalid,
    /// The text parses but the model no longer compiles it (`problems` model problems).
    CompileFailed {
        /// How many problems the compile reported.
        problems: usize,
    },
    /// The text compiles to another content hash than the one stored at publish.
    HashMismatch,
}

/// One version that did not load, by name and number. Never the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadFailure {
    /// The process name.
    pub name: String,
    /// The version number.
    pub version: u32,
    /// Why it did not load.
    pub kind: LoadFailureKind,
}

/// What [`load_into`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// The `(name, version)` pairs now in the registry, in load order.
    pub loaded: Vec<(String, u32)>,
    /// The versions that failed to load.
    pub failures: Vec<LoadFailure>,
}

impl LoadReport {
    /// Whether every published version loaded.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Compiles every published version in the store into `registry`.
///
/// Each stored text is parsed, given the stored version number and the `Published` status, and
/// compiled; the compiled content hash must equal the stored one. A version that fails any step
/// is listed in the report and not registered; the rest still load. A store failure aborts with
/// the error: a half-read store is not a clean start. Retired versions are not loaded.
///
/// # Errors
/// The store's error from `names` or `list`.
pub async fn load_into<S: DefinitionStore + ?Sized>(
    registry: &ProcessRegistry,
    store: &S,
) -> Result<LoadReport> {
    let mut report = LoadReport::default();
    for name in store.names().await? {
        let mut after = None;
        loop {
            let page = store.list(&name, after, MAX_PAGE).await?;
            let Some(last) = page.last().map(|v| v.version) else {
                break;
            };
            for stored in page {
                if stored.state != VersionState::Published {
                    continue;
                }
                let failure = |kind| LoadFailure {
                    name: stored.name.clone(),
                    version: stored.version,
                    kind,
                };
                let Ok(mut version) = from_text(&stored.text) else {
                    report.failures.push(failure(LoadFailureKind::TextInvalid));
                    continue;
                };
                version.version = stored.version;
                version.status = VersionStatus::Published;
                let definition = match ProcessDefinition::compile(&version) {
                    Ok(definition) => definition,
                    Err(problems) => {
                        report
                            .failures
                            .push(failure(LoadFailureKind::CompileFailed {
                                problems: problems.len(),
                            }));
                        continue;
                    }
                };
                if definition.compiled.hash.as_hex() != stored.content_hash
                    || version.name != stored.name
                {
                    report.failures.push(failure(LoadFailureKind::HashMismatch));
                    continue;
                }
                registry.publish(definition);
                report.loaded.push((stored.name.clone(), stored.version));
            }
            after = Some(last);
        }
    }
    Ok(report)
}
