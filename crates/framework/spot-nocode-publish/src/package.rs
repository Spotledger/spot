//! Packaging a designer's draft into the one `.spotapp` an installer accepts.
//!
//! The package has the ordinary bundle shape:
//!
//! - `manifest.toml` — a `spot-manifest`-valid, **declarative** app: no component code, minimal
//!   capabilities (the DocTypes it declares), its dependencies and its host range;
//! - `app.wasm` — the empty WebAssembly component. A no-code app has no functions of its own,
//!   but the bundle format (and the installer) need the component entry; the empty component
//!   imports nothing and exports nothing, so the privilege check sees no authority at all;
//! - `assets/nocode/customisation.spotc` — the site customisation bundle captured through
//!   `spot-custom-bundle` (custom fields, view layers, scripts, rules and declared fixtures);
//! - `assets/nocode/declarative.toml` — designed DocTypes and forms, processes and reports.
//!
//! Item selection is enforced while capturing: the capture reads a [`SelectedSite`] view of the
//! site that exposes only the items the draft names, so an unrelated script, rule, view or
//! fixture row can never leak into the app. Fixtures are the only document data a package may
//! carry, and only the fixture DocTypes the draft declares are visible.

use std::collections::BTreeMap;

use semver::Version;
use serde::Serialize;
use spot_bundle::{Bundle, BundleBuilder, HostInfo, Limits, Policy};
use spot_custom_bundle::{
    CaptureOptions, FixtureSet, RuleRecord, ScriptRecord, SectionRegistry, SiteReader, capture_with,
};
use spot_manifest::AppManifest;
use spot_meta::{Customisations, Definition};
use spot_nocode_model::{SiteContext, validate};
use spot_views::{ViewDef, ViewLayer};

use crate::declarative::{CUSTOMISATION_ASSET, DECLARATIVE_ASSET, DeclarativeApp};
use crate::draft::{AppDraft, ItemRef};
use crate::error::Error;

/// The empty WebAssembly component: magic, component-model version, layer 1, no sections.
///
/// It is a valid component with no imports and no exports. That is the honest component for an
/// app whose whole behaviour is declarative, and it keeps the bundle format uniform.
pub const EMPTY_COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";

/// A packed, validator-approved no-code app, before it is signed.
#[derive(Clone, Debug)]
pub struct BundleSource {
    /// The deterministic `.spotapp` bytes.
    bytes: Vec<u8>,
    /// The opened bundle (the installer's validator accepted it).
    bundle: Bundle,
    /// The parsed manifest the bytes carry.
    manifest: AppManifest,
    /// The declarative payload.
    declarative: DeclarativeApp,
    /// The captured customisation bundle bytes.
    customisation: Vec<u8>,
}

impl BundleSource {
    /// The `.spotapp` bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The opened and validated bundle.
    #[must_use]
    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    /// The parsed manifest.
    #[must_use]
    pub fn manifest(&self) -> &AppManifest {
        &self.manifest
    }

    /// The app's declarative body.
    #[must_use]
    pub fn declarative(&self) -> &DeclarativeApp {
        &self.declarative
    }

    /// The captured customisation bundle bytes.
    #[must_use]
    pub fn customisation(&self) -> &[u8] {
        &self.customisation
    }

    /// The canonical digest the publisher signs.
    #[must_use]
    pub fn digest(&self) -> spot_bundle::Digest {
        self.bundle.digest()
    }

    /// The publisher id the draft stated.
    #[must_use]
    pub fn publisher(&self) -> &str {
        &self.declarative.app.publisher
    }
}

/// The filtered view of a site the capture reads: only the draft's selected items.
struct SelectedSite<'a> {
    site: &'a dyn SiteReader,
    draft: &'a AppDraft,
    definitions: Vec<Definition>,
}

impl SelectedSite<'_> {
    /// Whether the draft selects a custom field.
    fn selects_field(&self, doctype: &str, field: &str) -> bool {
        self.draft.items.iter().any(|item| {
            matches!(
                item,
                ItemRef::CustomField {
                    doctype: d,
                    field: f
                } if d == doctype && f == field
            )
        })
    }

    /// Whether the draft selects a DocType-level property.
    fn selects_doctype(&self, doctype: &str) -> bool {
        self.draft
            .items
            .iter()
            .any(|item| matches!(item, ItemRef::DocType { name } if name == doctype))
    }

    /// Whether the draft selects a view layer for a DocType.
    fn selects_view(&self, doctype: &str) -> bool {
        self.draft
            .items
            .iter()
            .any(|item| matches!(item, ItemRef::View { doctype: d } if d == doctype))
    }
}

impl SiteReader for SelectedSite<'_> {
    fn installed_apps(&self) -> Vec<(String, Version)> {
        self.site.installed_apps()
    }

    fn host_version(&self) -> Version {
        self.site.host_version()
    }

    fn definitions(&self) -> Vec<Definition> {
        self.definitions.clone()
    }

    fn customisations(&self) -> Customisations {
        let source = self.site.customisations();
        let mut selected = Customisations::new();
        for field in source.fields() {
            if self.selects_field(&field.doctype, &field.spec.name) {
                selected.add_field(
                    &field.doctype,
                    field.spec.clone(),
                    field.insert_after.as_deref(),
                );
            }
        }
        for over in source.overrides() {
            let selected_override = match over.field.as_deref() {
                Some(field) => self.selects_field(&over.doctype, field),
                None => self.selects_doctype(&over.doctype),
            };
            if selected_override {
                selected.set(&over.doctype, over.field.as_deref(), over.property.clone());
            }
        }
        selected
    }

    fn view_layers(&self) -> Vec<ViewLayer> {
        self.site
            .view_layers()
            .into_iter()
            .filter(|layer| self.selects_view(&layer.doctype))
            .collect()
    }

    fn base_view(&self, doctype: &str) -> Option<ViewDef> {
        self.site.base_view(doctype)
    }

    fn scripts(&self) -> Vec<ScriptRecord> {
        self.site
            .scripts()
            .into_iter()
            .filter(|script| {
                self.draft
                    .items
                    .iter()
                    .any(|item| matches!(item, ItemRef::Script { name } if name == &script.name))
            })
            .collect()
    }

    fn rules(&self) -> Vec<RuleRecord> {
        self.site
            .rules()
            .into_iter()
            .filter(|rule| {
                self.draft
                    .items
                    .iter()
                    .any(|item| matches!(item, ItemRef::Rule { name } if name == &rule.name))
            })
            .collect()
    }

    fn fixtures(&self) -> Vec<FixtureSet> {
        let declared = self.draft.fixture_doctypes();
        self.site
            .fixtures()
            .into_iter()
            .filter(|set| declared.contains(&set.doctype))
            .collect()
    }

    fn is_system_managed(&self, kind: &str, item: &str) -> bool {
        self.site.is_system_managed(kind, item)
    }

    fn is_secret(&self, kind: &str, item: &str) -> bool {
        self.site.is_secret(kind, item)
    }
}

/// Packs `draft` from the site it was designed on.
///
/// # Errors
/// One [`Error`] naming the first problem: an invalid draft, a missing item, undeclared
/// fixture data, a malformed process or report, an invalid generated manifest, or a bundle the
/// installer's validator or the host policy refuses.
pub fn package(draft: &AppDraft, site: &dyn SiteReader) -> Result<BundleSource, Error> {
    package_with_policy(draft, site, &Policy::new())
}

/// Packages `draft` and checks it against a caller-supplied host policy.
///
/// The store operator's policy is the one that decides what a bundle may declare; a package
/// that the default policy accepts may still be over-privileged for a stricter store.
///
/// # Errors
/// As [`package`], plus [`Error::OverPrivileged`] when the policy refuses a declaration.
pub fn package_with_policy(
    draft: &AppDraft,
    site: &dyn SiteReader,
    policy: &Policy,
) -> Result<BundleSource, Error> {
    draft.validate()?;
    verify_items(draft, site)?;
    let definitions = designed_definitions(draft, site)?;
    let selected = SelectedSite {
        site,
        draft,
        definitions: definitions.clone(),
    };

    let options = CaptureOptions::new(
        draft.name.clone(),
        draft.version.to_string(),
        draft.publisher.clone(),
        draft.created_at.clone(),
    );
    let captured = capture_with(
        &selected,
        &options,
        &SectionRegistry::new(),
        &spot_custom_bundle::Limits::default(),
    )
    .map_err(|error| Error::CustomBundle {
        message: format!("{}: {error}", error.code()),
    })?;
    let customisation = captured.to_bytes();
    ensure_declared_fixtures(draft, &captured)?;

    let mut forms = Vec::new();
    let mut doctypes = Vec::new();
    for designed in &draft.drafts {
        doctypes.push(designed.doctype_def.clone());
        forms.push(designed.form_layout.to_view(designed.doctype()));
        for child in &designed.children {
            doctypes.push(child.doctype_def.clone());
            forms.push(child.form_layout.to_view(&child.doctype_def.name));
        }
    }
    let declarative = DeclarativeApp::from_draft(draft, doctypes, forms);
    let declarative_toml = declarative.to_toml()?;
    let manifest_source = manifest_toml(draft)?;
    spot_manifest::parse(&manifest_source, "manifest.toml").map_err(|errors| Error::Manifest {
        messages: errors
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
    })?;

    let bytes = BundleBuilder::new(manifest_source, EMPTY_COMPONENT.to_vec())
        .asset(CUSTOMISATION_ASSET, customisation.clone())
        .asset(DECLARATIVE_ASSET, declarative_toml.into_bytes())
        .build()
        .map_err(Error::bundle)?;

    let host = HostInfo {
        version: site.host_version(),
    };
    let bundle = Bundle::open(&bytes, &Limits::default(), &host).map_err(Error::bundle)?;
    let report = bundle.check_privileges(policy);
    if !report.is_ok() {
        return Err(Error::over_privileged(report.into_errors()));
    }
    let manifest = bundle.manifest().clone();
    Ok(BundleSource {
        bytes,
        bundle,
        manifest,
        declarative,
        customisation,
    })
}

/// Checks every selected item really exists on the site (or is designed in a draft).
fn verify_items(draft: &AppDraft, site: &dyn SiteReader) -> Result<(), Error> {
    let definitions = site.definitions();
    let designed: Vec<&str> = draft.drafts.iter().map(|d| d.doctype()).collect();
    let customisations = site.customisations();
    let views = site.view_layers();
    let scripts = site.scripts();
    let rules = site.rules();
    let fixtures = site.fixtures();
    for item in &draft.items {
        let present = match item {
            ItemRef::DocType { name } => {
                definitions.iter().any(|d| &d.name == name) || designed.contains(&name.as_str())
            }
            ItemRef::CustomField { doctype, field } => customisations
                .fields()
                .iter()
                .any(|f| &f.doctype == doctype && &f.spec.name == field),
            ItemRef::View { doctype } => {
                views.iter().any(|layer| &layer.doctype == doctype)
                    || draft.drafts.iter().any(|d| d.doctype() == doctype)
            }
            ItemRef::Script { name } => scripts.iter().any(|s| &s.name == name),
            ItemRef::Rule { name } => rules.iter().any(|r| &r.name == name),
            ItemRef::Fixture { doctype } => fixtures.iter().any(|f| &f.doctype == doctype),
            ItemRef::Process { name, source } | ItemRef::Report { name, source } => {
                if name.trim().is_empty() || source.trim().is_empty() {
                    return Err(Error::draft(format!(
                        "the {} definition has no name or no source",
                        item.kind()
                    )));
                }
                if toml::from_str::<toml::Value>(source).is_err()
                    && serde_json::from_str::<serde_json::Value>(source).is_err()
                {
                    return Err(Error::draft(format!(
                        "the {} `{name}` is neither TOML nor JSON",
                        item.kind()
                    )));
                }
                true
            }
        };
        if !present {
            return Err(Error::MissingItem {
                item: format!("{}:{}", item.kind(), item.key()),
            });
        }
    }
    Ok(())
}

/// The definitions the package carries: the site's, with the designed ones replacing a
/// same-named draft target, exactly as the designer's preview resolves them.
fn designed_definitions(draft: &AppDraft, site: &dyn SiteReader) -> Result<Vec<Definition>, Error> {
    let site_id = spot_core::SiteId::new(&draft.site)
        .map_err(|error| Error::draft(format!("the site id is not usable: {error}")))?;
    let context = SiteContext::new(site_id).with_definitions(site.definitions());
    let designed_names: Vec<&str> = draft.drafts.iter().map(|d| d.doctype()).collect();
    let mut definitions: Vec<Definition> = site
        .definitions()
        .into_iter()
        .filter(|definition| !designed_names.contains(&definition.name.as_str()))
        .collect();
    for designed in &draft.drafts {
        let report = validate(designed, &context);
        if !report.is_ok() {
            let issues = report
                .errors()
                .map(|issue| issue.message.clone())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Error::draft(format!(
                "the DocType `{}` is not valid: {issues}",
                designed.doctype()
            )));
        }
        definitions.push(designed.doctype_def.clone());
        for child in &designed.children {
            definitions.push(child.doctype_def.clone());
        }
    }
    Ok(definitions)
}

/// Fixtures are the only rows a package may carry; every captured set must be declared.
fn ensure_declared_fixtures(
    draft: &AppDraft,
    captured: &spot_custom_bundle::Bundle,
) -> Result<(), Error> {
    let declared = draft.fixture_doctypes();
    if let Some(section) = captured.section("fixtures") {
        for name in section.files.keys() {
            let doctype = name.strip_suffix(".json").unwrap_or(name);
            if !declared.contains(doctype) {
                return Err(Error::DataNotDeclared {
                    doctype: doctype.to_owned(),
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The generated manifest
// ---------------------------------------------------------------------------------------------

/// The `[app]` table.
#[derive(Serialize)]
struct ManifestApp {
    name: String,
    version: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    requires_host: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    depends: Vec<ManifestDependency>,
}

/// One `[[app.depends]]` row.
#[derive(Serialize)]
struct ManifestDependency {
    app: String,
    version: String,
}

/// One `[[assets]]` row.
#[derive(Serialize)]
struct ManifestAsset {
    path: String,
}

/// `[capabilities]`: the DocTypes the app declares, and nothing else.
#[derive(Serialize)]
struct ManifestCapabilities {
    doctypes: ManifestDocTypes,
}

/// `[capabilities.doctypes]`.
#[derive(Serialize)]
struct ManifestDocTypes {
    read: Vec<String>,
    write: Vec<String>,
}

/// The manifest document this crate writes.
#[derive(Serialize)]
struct ManifestDocument {
    app: ManifestApp,
    assets: Vec<ManifestAsset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<ManifestCapabilities>,
}

/// Renders the declarative app manifest.
fn manifest_toml(draft: &AppDraft) -> Result<String, Error> {
    let doctypes = draft.doctypes();
    let assets = vec![
        ManifestAsset {
            path: CUSTOMISATION_ASSET.to_owned(),
        },
        ManifestAsset {
            path: DECLARATIVE_ASSET.to_owned(),
        },
    ];
    let capabilities = if doctypes.is_empty() {
        None
    } else {
        Some(ManifestCapabilities {
            doctypes: ManifestDocTypes {
                read: doctypes.clone(),
                write: doctypes,
            },
        })
    };
    let document = ManifestDocument {
        app: ManifestApp {
            name: draft.name.clone(),
            version: draft.version.to_string(),
            title: draft.title.clone(),
            requires_host: draft
                .requires_host
                .as_ref()
                .map(std::string::ToString::to_string),
            depends: draft
                .dependencies
                .iter()
                .map(|dependency| ManifestDependency {
                    app: dependency.app.clone(),
                    version: dependency.version.to_string(),
                })
                .collect(),
        },
        assets,
        capabilities,
    };
    toml::to_string_pretty(&document)
        .map_err(|error| Error::draft(format!("the manifest cannot be written: {error}")))
}

/// The declared asset paths, for callers that check what a package carries.
#[must_use]
pub fn asset_paths() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("customisation", CUSTOMISATION_ASSET),
        ("declarative", DECLARATIVE_ASSET),
    ])
}
