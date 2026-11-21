//! Capturing a site's customizations into a bundle.

use std::collections::BTreeMap;

use crate::bundle::{Bundle, SectionData};
use crate::error::{CustomBundleError, codes};
use crate::header::AppVersion;
use crate::limits::Limits;
use crate::registry::{SectionRegistry, SiteReader};

/// The bundle metadata capture takes as arguments (time is never read from the clock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureOptions {
    /// The bundle's name.
    pub name: String,
    /// The bundle's own version (semver).
    pub bundle_version: String,
    /// Who created it.
    pub created_by: String,
    /// When it was created, as the caller states it.
    pub created_at: String,
}

impl CaptureOptions {
    /// Options with the given metadata.
    pub fn new(
        name: impl Into<String>,
        bundle_version: impl Into<String>,
        created_by: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            bundle_version: bundle_version.into(),
            created_by: created_by.into(),
            created_at: created_at.into(),
        }
    }
}

/// The error code for a section's `check`, so a captured bundle that fails its own validation
/// is refused with the right code.
fn section_code(kind: &str) -> &'static str {
    match kind {
        "meta" => codes::INVALID_META,
        "views" => codes::INVALID_VIEWS,
        "scripts" => codes::INVALID_SCRIPTS,
        "rules" => codes::INVALID_RULES,
        "fixtures" => codes::INVALID_FIXTURES,
        _ => codes::APPLY_FAILED,
    }
}

/// The section map captured from a site: every section with files, checked.
pub(crate) fn captured_sections(
    site: &dyn SiteReader,
    registry: &SectionRegistry,
) -> Result<BTreeMap<String, SectionData>, CustomBundleError> {
    let mut sections = BTreeMap::new();
    for section in registry.iter() {
        let files = section.capture(site);
        if files.is_empty() {
            continue;
        }
        if let Some(problem) = section.check(&files).into_iter().next() {
            return Err(CustomBundleError::about(
                section_code(section.kind()),
                problem.item,
                problem.message,
            ));
        }
        sections.insert(
            section.kind().to_owned(),
            SectionData {
                schema: section.schema_version(),
                files,
            },
        );
    }
    Ok(sections)
}

/// Captures a site into a bundle with the built-in section registry and default limits.
///
/// Capture includes only what differs from the installed apps: a property override equal to
/// the app's own value is not carried, and neither is anything the site marks system-managed
/// or secret. The bundle refuses to pack if a section fails its own check (a rule that does
/// not parse, a script whose hash does not match, a malformed fixture).
pub fn capture(
    site: &dyn SiteReader,
    options: &CaptureOptions,
) -> Result<Bundle, CustomBundleError> {
    capture_with(site, options, &SectionRegistry::new(), &Limits::default())
}

/// As [`capture`], with a caller-supplied registry (a crate that registered a later section)
/// and limits.
pub fn capture_with(
    site: &dyn SiteReader,
    options: &CaptureOptions,
    registry: &SectionRegistry,
    limits: &Limits,
) -> Result<Bundle, CustomBundleError> {
    let sections = captured_sections(site, registry)?;
    let captured_against: Vec<AppVersion> = site
        .installed_apps()
        .into_iter()
        .map(|(app, version)| AppVersion {
            app,
            version: version.to_string(),
        })
        .collect();
    let header = crate::header::BundleHeader::new(
        options.name.clone(),
        options.bundle_version.clone(),
        options.created_by.clone(),
        options.created_at.clone(),
        site.host_version().to_string(),
        captured_against,
    );
    Bundle::pack(header, sections, limits)
}
