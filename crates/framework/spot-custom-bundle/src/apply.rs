//! Planning and applying a bundle: [`plan_apply`] validates and produces typed changes,
//! [`apply`] executes them in one transaction, and [`diff`] is the plain structural diff.

use std::collections::{BTreeMap, BTreeSet};

use spot_meta::{Customisations, Registry};

use crate::bundle::Bundle;
use crate::digest::sha256_hex;
use crate::error::{CustomBundleError, codes};
use crate::registry::{
    Change, Files, Problem, SectionRegistry, SiteReader, SiteWriter, plan_between,
};
use crate::section::{meta, scripts, views};

/// The validation and the typed changes for applying one bundle to one site.
#[derive(Clone, Debug)]
pub struct ApplyPlan {
    changes: Vec<Change>,
    warnings: Vec<Problem>,
    files: BTreeMap<String, Files>,
    digest: crate::digest::Digest,
    hash: String,
}

impl ApplyPlan {
    /// The changes, in section then key order.
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// The compatibility warnings (a minor or patch app version difference).
    pub fn warnings(&self) -> &[Problem] {
        &self.warnings
    }

    /// Whether the plan would change nothing.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The digest of the bundle this plan was built for.
    pub fn digest(&self) -> crate::digest::Digest {
        self.digest
    }

    /// The plan's deterministic hash: SHA-256 over the bundle digest and every change.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// The bundle's files, by section kind, that [`apply`] reads from.
    pub fn files(&self) -> &BTreeMap<String, Files> {
        &self.files
    }
}

/// What an apply did.
#[derive(Clone, Debug)]
pub struct ApplyReport {
    applied: Vec<Change>,
    warnings: Vec<Problem>,
}

impl ApplyReport {
    /// The changes that were written.
    pub fn applied(&self) -> &[Change] {
        &self.applied
    }

    /// The plan's warnings.
    pub fn warnings(&self) -> &[Problem] {
        &self.warnings
    }
}

/// Maps a `spot_meta` error kind onto the catalogue code an operator can look up.
fn meta_error_code(kind: spot_meta::MetaErrorKind) -> &'static str {
    use spot_meta::MetaErrorKind;
    match kind {
        MetaErrorKind::UnknownName => "meta.unknown_doctype",
        _ => "meta.validation_failed",
    }
}

/// The app that owns a DocType, from the site's definitions.
fn app_of<'a>(definitions: &'a [spot_meta::Definition], doctype: &str) -> Option<&'a str> {
    definitions
        .iter()
        .find(|definition| definition.name == doctype)
        .and_then(|definition| definition.app.as_deref())
}

/// Validates a bundle against a site: the metadata rebuilds, every view layer resolves and
/// validates, every rule parses. Returns nothing; the plan is produced afterwards.
fn validate(
    bundle: &Bundle,
    site: &dyn SiteReader,
    registry: &SectionRegistry,
) -> Result<(), Vec<CustomBundleError>> {
    let definitions = site.definitions();
    let customisations = match bundle.section("meta") {
        Some(data) => meta::customisations_from_files(&data.files).map_err(|err| vec![err])?,
        None => Customisations::new(),
    };
    let effective = match Registry::build(definitions.clone(), &customisations) {
        Ok(effective) => effective,
        Err(errors) => {
            return Err(errors
                .into_iter()
                .map(|error| {
                    let item = match (&error.doctype, &error.field) {
                        (Some(doctype), Some(field)) => format!("{doctype}.{field}"),
                        (Some(doctype), None) => doctype.clone(),
                        (None, Some(field)) => field.clone(),
                        (None, None) => String::new(),
                    };
                    CustomBundleError::about(
                        codes::INVALID_META,
                        item,
                        format!("{}: {error}", meta_error_code(error.kind)),
                    )
                })
                .collect());
        }
    };

    if let Some(data) = bundle.section("views") {
        let layers = views::layers_from_files(&data.files).map_err(|problems| {
            problems
                .into_iter()
                .map(CustomBundleError::from_problem)
                .collect::<Vec<_>>()
        })?;
        let mut by_doctype: BTreeMap<&str, Vec<spot_views::ViewLayer>> = BTreeMap::new();
        for layer in &layers {
            by_doctype
                .entry(layer.doctype.as_str())
                .or_default()
                .push(layer.clone());
        }
        let mut errors = Vec::new();
        for (doctype, layers) in by_doctype {
            let Some(base) = site.base_view(doctype) else {
                errors.push(CustomBundleError::about(
                    codes::INVALID_VIEWS,
                    doctype,
                    "the site has no base view for this DocType; a layer cannot be resolved",
                ));
                continue;
            };
            let resolved = spot_views::resolve(&base, &layers, &effective);
            for error in spot_views::validate_layer(&resolved, &effective) {
                errors.push(CustomBundleError::about(
                    codes::INVALID_VIEWS,
                    doctype,
                    format!("{}: {error}", error.kind.code()),
                ));
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
    }

    // Every other section's own check (rules parse, scripts match, fixtures are JSON).
    let problems = bundle.check(registry);
    if !problems.is_empty() {
        return Err(problems
            .into_iter()
            .map(CustomBundleError::from_problem)
            .collect());
    }

    Ok(())
}

/// The apps a bundle customises: the owners of the DocTypes it touches plus each script's app.
fn customised_apps(bundle: &Bundle, site: &dyn SiteReader) -> BTreeSet<String> {
    let definitions = site.definitions();
    let mut apps = BTreeSet::new();
    let mut note = |doctype: &str| {
        if let Some(app) = app_of(&definitions, doctype) {
            apps.insert(app.to_owned());
        }
    };
    if let Some(data) = bundle.section("meta")
        && let Ok(customisations) = meta::customisations_from_files(&data.files)
    {
        for field in customisations.fields() {
            note(&field.doctype);
        }
        for over in customisations.overrides() {
            note(&over.doctype);
        }
    }
    if let Some(data) = bundle.section("views")
        && let Ok(layers) = views::layers_from_files(&data.files)
    {
        for layer in layers {
            note(&layer.doctype);
        }
    }
    if let Some(data) = bundle.section("fixtures") {
        for path in data.files.keys() {
            if let Some(doctype) = path.strip_suffix(".json") {
                note(doctype);
            }
        }
    }
    if let Some(data) = bundle.section("scripts") {
        for app in scripts::script_apps(&data.files) {
            if !app.is_empty() {
                apps.insert(app);
            }
        }
    }
    apps
}

/// Checks the bundle's `captured_against` against the target's installed apps.
fn compatibility(
    bundle: &Bundle,
    site: &dyn SiteReader,
) -> Result<Vec<Problem>, Vec<CustomBundleError>> {
    let captured: BTreeMap<&str, &str> = bundle
        .header()
        .captured_against
        .iter()
        .map(|entry| (entry.app.as_str(), entry.version.as_str()))
        .collect();
    let installed: BTreeMap<String, semver::Version> = site.installed_apps().into_iter().collect();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for app in customised_apps(bundle, site) {
        let Some(captured_version) = captured.get(app.as_str()) else {
            continue;
        };
        let Ok(captured_version) = captured_version.parse::<semver::Version>() else {
            continue;
        };
        let Some(target) = installed.get(&app) else {
            errors.push(CustomBundleError::about(
                codes::APP_MISSING,
                app,
                "the app the bundle customises is not installed on the target",
            ));
            continue;
        };
        if target.major != captured_version.major {
            errors.push(CustomBundleError::about(
                codes::APP_INCOMPATIBLE,
                app,
                format!(
                    "the app is at major {} on the target but the bundle was captured against {}",
                    target.major, captured_version.major
                ),
            ));
        } else if target != &captured_version {
            warnings.push(Problem {
                code: codes::APP_VERSION.to_owned(),
                item: app,
                message: format!(
                    "the app is {target} on the target but the bundle was captured against \
                     {captured_version}"
                ),
            });
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(warnings)
}

/// Validates a bundle against a target site and produces the typed plan.
///
/// Nothing is written: this is the "plan" of ADR-22. It refuses (`Err`) on a meta rebuild
/// failure, an unresolvable or invalid view layer, a rule that does not parse, a script whose
/// hash does not match, an app missing or incompatible, or an item the target marks
/// system-managed or secret.
pub fn plan_apply(
    bundle: &Bundle,
    site: &dyn SiteReader,
    registry: &SectionRegistry,
) -> Result<ApplyPlan, Vec<CustomBundleError>> {
    validate(bundle, site, registry)?;
    let warnings = compatibility(bundle, site)?;

    let mut files: BTreeMap<String, Files> = BTreeMap::new();
    let mut changes: Vec<Change> = Vec::new();
    for section in registry.iter() {
        let section_files = bundle
            .section(section.kind())
            .map(|data| data.files.clone())
            .unwrap_or_default();
        // Refuse to apply onto, or through, anything the target protects.
        for key in section.items(&section_files).keys() {
            if site.is_system_managed(section.kind(), key) || site.is_secret(section.kind(), key) {
                return Err(vec![CustomBundleError::about(
                    codes::SYSTEM_MANAGED,
                    key.clone(),
                    "the target marks this item system-managed or secret",
                )]);
            }
        }
        changes.extend(section.plan_apply(&section_files, site));
        files.insert(section.kind().to_owned(), section_files);
    }

    let hash = plan_hash(bundle.digest(), &changes);
    Ok(ApplyPlan {
        changes,
        warnings,
        files,
        digest: bundle.digest(),
        hash,
    })
}

/// SHA-256 over the bundle digest and every change, for a stable plan identity.
fn plan_hash(digest: crate::digest::Digest, changes: &[Change]) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(crate::digest::DIGEST_PREFIX);
    input.extend_from_slice(b"plan\0");
    input.extend_from_slice(digest.to_hex().as_bytes());
    for change in changes {
        input.push(0);
        input.extend_from_slice(change.section.as_bytes());
        input.push(b':');
        input.extend_from_slice(change.key.as_bytes());
        input.push(b':');
        input.extend_from_slice(change.hash.as_bytes());
    }
    sha256_hex(&input)
}

/// Applies a plan to a site in one transaction.
///
/// Every change is written through `site`'s buffered transaction; if any fails, the
/// transaction is rolled back and the site is left unchanged. Applying the same plan twice is
/// what makes the second plan empty: the state the plan describes is exactly what apply writes.
pub fn apply(
    plan: &ApplyPlan,
    site: &mut dyn SiteWriter,
    registry: &SectionRegistry,
) -> Result<ApplyReport, CustomBundleError> {
    site.begin()?;
    for change in &plan.changes {
        let Some(section) = registry.get(&change.section) else {
            site.rollback()?;
            return Err(CustomBundleError::about(
                codes::APPLY_FAILED,
                &change.section,
                "the plan names a section the registry does not know",
            ));
        };
        let empty = Files::new();
        let files = plan.files.get(&change.section).unwrap_or(&empty);
        if let Err(err) = section.apply(change, files, site) {
            site.rollback()?;
            return Err(err);
        }
    }
    site.commit()?;
    Ok(ApplyReport {
        applied: plan.changes.clone(),
        warnings: plan.warnings.clone(),
    })
}

/// The plain structural diff from bundle `a` to bundle `b`: the items `b` adds, changes or
/// drops relative to `a`. This is ALM-03's input, not its merge logic.
pub fn diff(a: &Bundle, b: &Bundle, registry: &SectionRegistry) -> Vec<Change> {
    let mut kinds: BTreeSet<String> = a.sections().keys().cloned().collect();
    kinds.extend(b.sections().keys().cloned());
    let empty = Files::new();
    let mut changes = Vec::new();
    for kind in kinds {
        let Some(section) = registry.get(&kind) else {
            // An unknown section: compare at file granularity.
            let a_files = a.section(&kind).map(|data| &data.files).unwrap_or(&empty);
            let b_files = b.section(&kind).map(|data| &data.files).unwrap_or(&empty);
            let current: BTreeMap<String, Vec<u8>> = a_files
                .iter()
                .map(|(name, bytes)| (name.clone(), bytes.clone()))
                .collect();
            let desired: BTreeMap<String, Vec<u8>> = b_files
                .iter()
                .map(|(name, bytes)| (name.clone(), bytes.clone()))
                .collect();
            changes.extend(plan_between(&kind, &current, &desired, |_| {
                crate::registry::Risk::Medium
            }));
            continue;
        };
        let a_files = a.section(&kind).map(|data| &data.files).unwrap_or(&empty);
        let b_files = b.section(&kind).map(|data| &data.files).unwrap_or(&empty);
        let current = section.items(a_files);
        let desired = section.items(b_files);
        changes.extend(plan_between(&kind, &current, &desired, |action| {
            section.risk(action)
        }));
    }
    changes
}
