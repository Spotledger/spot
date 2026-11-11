//! The input to one detection run: the release plan and the site's customisations.
//!
//! Nothing here reads a database, a clock or the environment: a caller (the runtime, a
//! rehearsal clone, a test) supplies every value. The release plan is the one
//! `spot-migration-plan` produced; the site is described by its customisations, its view layers,
//! its scripts, rules and fixtures, and the apps installed on it; the new metadata is the
//! `spot_meta::EffectiveMeta` the release will publish.

use spot_manifest::AppManifest;
use spot_meta::{Customisations, Definition, EffectiveMeta};
use spot_migration_plan::ReleasePlan;
use spot_views::{ViewDef, ViewLayer};

/// One script on the site: its TypeScript source and the DocType and hook point it is bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteScript {
    /// The script's name within its app.
    pub name: String,
    /// The app that owns the script.
    pub app: String,
    /// The DocType the script's `doc` argument is, when the script is bound to one. Without it
    /// the script's field references cannot be checked against the declarations.
    pub doctype: Option<String>,
    /// The hook point the script runs at, as the manifest spells it (`validate_save`, `load`).
    pub hook_point: String,
    /// The TypeScript source, checked but never executed.
    pub source: String,
}

impl SiteScript {
    /// The script's stable id, `<app>:<name>`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}:{}", self.app, self.name)
    }
}

/// One rule on the site: a name, the DocType it belongs to and its expression text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteRule {
    /// The rule's name.
    pub name: String,
    /// The DocType whose fields the expression reads, when it is bound to one.
    pub doctype: Option<String>,
    /// The expression text, parsed by `spot-expr`.
    pub expression: String,
}

/// One DocType's fixtures: the JSON documents a site added.
#[derive(Clone, Debug, PartialEq)]
pub struct SiteFixture {
    /// The DocType the documents belong to.
    pub doctype: String,
    /// The documents, as the JSON the fixture loader reads.
    pub documents: Vec<serde_json::Value>,
}

/// One installed app: its parsed manifest and the versioned WIT world its bundle targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledApp {
    /// The parsed, validated manifest (`spot-manifest`).
    pub manifest: AppManifest,
    /// The versioned WIT world the app's bundle targets, as `spot-installer`'s `Candidate`
    /// carries it.
    pub wit_world: String,
}

impl InstalledApp {
    /// An installed app from its manifest and WIT world.
    #[must_use]
    pub fn new(manifest: AppManifest, wit_world: impl Into<String>) -> Self {
        Self {
            manifest,
            wit_world: wit_world.into(),
        }
    }

    /// The app's storage name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.manifest.app.name.as_str()
    }
}

/// The new host a release publishes: its version and the WIT world it serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSpec {
    /// The host's platform version, checked against each app's `requires_host`.
    pub version: semver::Version,
    /// The WIT world the new host serves, checked against each app's world.
    pub wit_world: String,
}

impl HostSpec {
    /// A host at `version` serving `wit_world`.
    #[must_use]
    pub fn new(version: semver::Version, wit_world: impl Into<String>) -> Self {
        Self {
            version,
            wit_world: wit_world.into(),
        }
    }
}

/// Everything one detection run needs.
///
/// Build it with [`ReleaseContext::new`] and the `with_*` setters; every optional part defaults
/// to empty, so a test that only exercises custom fields names only the plan and the metadata.
pub struct ReleaseContext<'a> {
    /// The release plan (WP-MIG-01): the release items this run reasons about.
    pub plan: &'a ReleasePlan,
    /// The new app definitions, **without** the site's customisations: what a custom field may
    /// collide with and what a site `options` override must match.
    pub new_standard: &'a [Definition],
    /// The pre-release app definitions, when the caller has them. They let the `options` check
    /// tell a value the release *retired* (a conflict when a site override keeps it) from a value
    /// the site itself added (never a conflict). `None` skips the retired-value check.
    pub old_standard: Option<&'a [Definition]>,
    /// The new **effective** metadata (app definitions merged with the site's customisations),
    /// what the views resolve against and the declarations are generated from.
    pub new_meta: &'a EffectiveMeta,
    /// The site's custom fields and property overrides.
    pub customisations: &'a Customisations,
    /// The site's view layers.
    pub view_layers: &'a [ViewLayer],
    /// The base views the layers diff over, one per DocType (as `spot-views` resolves them).
    pub base_views: &'a [ViewDef],
    /// The site's scripts.
    pub scripts: &'a [SiteScript],
    /// The site's rules.
    pub rules: &'a [SiteRule],
    /// The site's fixtures.
    pub fixtures: &'a [SiteFixture],
    /// The apps installed on the site.
    pub apps: &'a [InstalledApp],
    /// The new host, when the run should check app host and WIT compatibility.
    pub host: Option<&'a HostSpec>,
}

impl<'a> ReleaseContext<'a> {
    /// A context with the four required parts and everything optional empty.
    #[must_use]
    pub fn new(
        plan: &'a ReleasePlan,
        new_standard: &'a [Definition],
        new_meta: &'a EffectiveMeta,
        customisations: &'a Customisations,
    ) -> Self {
        Self {
            plan,
            new_standard,
            old_standard: None,
            new_meta,
            customisations,
            view_layers: &[],
            base_views: &[],
            scripts: &[],
            rules: &[],
            fixtures: &[],
            apps: &[],
            host: None,
        }
    }

    /// Sets the pre-release app definitions, enabling the retired-option-value check.
    #[must_use]
    pub fn with_old_standard(mut self, old_standard: &'a [Definition]) -> Self {
        self.old_standard = Some(old_standard);
        self
    }

    /// Sets the view layers and their base views.
    #[must_use]
    pub fn with_views(mut self, layers: &'a [ViewLayer], bases: &'a [ViewDef]) -> Self {
        self.view_layers = layers;
        self.base_views = bases;
        self
    }

    /// Sets the scripts.
    #[must_use]
    pub fn with_scripts(mut self, scripts: &'a [SiteScript]) -> Self {
        self.scripts = scripts;
        self
    }

    /// Sets the rules.
    #[must_use]
    pub fn with_rules(mut self, rules: &'a [SiteRule]) -> Self {
        self.rules = rules;
        self
    }

    /// Sets the fixtures.
    #[must_use]
    pub fn with_fixtures(mut self, fixtures: &'a [SiteFixture]) -> Self {
        self.fixtures = fixtures;
        self
    }

    /// Sets the installed apps and the new host.
    #[must_use]
    pub fn with_apps(mut self, apps: &'a [InstalledApp], host: &'a HostSpec) -> Self {
        self.apps = apps;
        self.host = Some(host);
        self
    }

    /// Detects every conflict this release causes on the site.
    #[must_use]
    pub fn detect(&self) -> crate::ConflictReport {
        crate::detect(self)
    }
}
