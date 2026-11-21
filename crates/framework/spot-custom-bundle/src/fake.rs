//! An in-memory site: [`FakeSite`] implements both [`SiteReader`] and [`SiteWriter`], so a
//! capture, a plan and an apply can be exercised without a database or a network.
//!
//! It is the contract-suite site and the round-trip test's both ends: capture site A, apply to
//! an empty B, capture B, and the two bundles are byte-identical.

use std::collections::{BTreeMap, BTreeSet};

use spot_meta::{Customisations, Definition, FieldSpec, Property};
use spot_views::{LayerKind, ViewDef, ViewLayer};

use crate::error::CustomBundleError;
use crate::registry::{FixtureSet, RuleRecord, ScriptRecord, SiteReader, SiteWriter};

/// A mutable in-memory site.
#[derive(Clone, Debug)]
pub struct FakeSite {
    apps: Vec<(String, semver::Version)>,
    host: semver::Version,
    definitions: Vec<Definition>,
    customisations: Customisations,
    base_views: BTreeMap<String, ViewDef>,
    layers: Vec<ViewLayer>,
    scripts: BTreeMap<String, ScriptRecord>,
    rules: BTreeMap<String, String>,
    fixtures: BTreeMap<String, Vec<serde_json::Value>>,
    system_managed: BTreeSet<String>,
    secrets: BTreeSet<String>,
    snapshot: Option<Snapshot>,
}

/// The mutable state a transaction restores.
#[derive(Clone, Debug)]
struct Snapshot {
    customisations: Customisations,
    layers: Vec<ViewLayer>,
    scripts: BTreeMap<String, ScriptRecord>,
    rules: BTreeMap<String, String>,
    fixtures: BTreeMap<String, Vec<serde_json::Value>>,
}

impl Default for FakeSite {
    fn default() -> Self {
        Self {
            apps: Vec::new(),
            host: semver::Version::new(0, 1, 0),
            definitions: Vec::new(),
            customisations: Customisations::new(),
            base_views: BTreeMap::new(),
            layers: Vec::new(),
            scripts: BTreeMap::new(),
            rules: BTreeMap::new(),
            fixtures: BTreeMap::new(),
            system_managed: BTreeSet::new(),
            secrets: BTreeSet::new(),
            snapshot: None,
        }
    }
}

/// The `kind:item` key an exclusion set stores.
fn exclusion_key(kind: &str, item: &str) -> String {
    format!("{kind}:{item}")
}

impl FakeSite {
    /// An empty site on host `0.1.0`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an installed app.
    pub fn with_app(mut self, app: impl Into<String>, version: semver::Version) -> Self {
        self.apps.push((app.into(), version));
        self
    }

    /// Sets the host version.
    pub fn with_host(mut self, host: semver::Version) -> Self {
        self.host = host;
        self
    }

    /// Adds a DocType definition.
    pub fn with_definition(mut self, definition: Definition) -> Self {
        self.definitions.push(definition);
        self
    }

    /// Adds a base (app) view.
    pub fn with_base_view(mut self, view: ViewDef) -> Self {
        self.base_views.insert(view.doctype.clone(), view);
        self
    }

    /// Adds a view layer.
    pub fn with_layer(mut self, layer: ViewLayer) -> Self {
        self.layers.push(layer);
        self
    }

    /// Adds a custom field.
    pub fn with_custom_field(
        mut self,
        doctype: &str,
        spec: FieldSpec,
        insert_after: Option<&str>,
    ) -> Self {
        self.customisations.add_field(doctype, spec, insert_after);
        self
    }

    /// Sets a property override.
    pub fn with_override(mut self, doctype: &str, field: Option<&str>, property: Property) -> Self {
        self.customisations.set(doctype, field, property);
        self
    }

    /// Adds a script, recording the SHA-256 of its source as its hash.
    pub fn with_script(mut self, name: &str, app: &str, hook_point: &str, source: &str) -> Self {
        self.scripts.insert(
            name.to_owned(),
            ScriptRecord {
                name: name.to_owned(),
                app: app.to_owned(),
                hook_point: hook_point.to_owned(),
                source: source.to_owned(),
                hash: crate::digest::sha256_hex(source.as_bytes()),
            },
        );
        self
    }

    /// Adds a rule.
    pub fn with_rule(mut self, name: &str, expression: &str) -> Self {
        self.rules.insert(name.to_owned(), expression.to_owned());
        self
    }

    /// Adds a DocType's fixtures.
    pub fn with_fixtures(mut self, doctype: &str, documents: Vec<serde_json::Value>) -> Self {
        self.fixtures.insert(doctype.to_owned(), documents);
        self
    }

    /// Marks an item system-managed: capture skips it and apply refuses to touch it.
    pub fn mark_system_managed(mut self, kind: &str, item: &str) -> Self {
        self.system_managed.insert(exclusion_key(kind, item));
        self
    }

    /// Marks an item secret: capture skips it and apply refuses to touch it.
    pub fn mark_secret(mut self, kind: &str, item: &str) -> Self {
        self.secrets.insert(exclusion_key(kind, item));
        self
    }

    /// The site's customisations, for assertions.
    pub fn customisations(&self) -> &Customisations {
        &self.customisations
    }

    /// The site's layers, for assertions.
    pub fn layers(&self) -> &[ViewLayer] {
        &self.layers
    }

    /// Replaces the field's spec, keeping the overrides that name it.
    fn replace_field(&mut self, doctype: &str, spec: FieldSpec, insert_after: Option<&str>) {
        let old = std::mem::take(&mut self.customisations);
        let mut rebuilt = Customisations::new();
        for field in old.fields() {
            if !(field.doctype == doctype && field.spec.name == spec.name) {
                rebuilt.add_field(
                    &field.doctype,
                    field.spec.clone(),
                    field.insert_after.as_deref(),
                );
            }
        }
        rebuilt.add_field(doctype, spec, insert_after);
        for over in old.overrides() {
            rebuilt.set(&over.doctype, over.field.as_deref(), over.property.clone());
        }
        self.customisations = rebuilt;
    }
}

impl SiteReader for FakeSite {
    fn installed_apps(&self) -> Vec<(String, semver::Version)> {
        self.apps.clone()
    }

    fn host_version(&self) -> semver::Version {
        self.host.clone()
    }

    fn definitions(&self) -> Vec<Definition> {
        self.definitions.clone()
    }

    fn customisations(&self) -> Customisations {
        self.customisations.clone()
    }

    fn view_layers(&self) -> Vec<ViewLayer> {
        self.layers.clone()
    }

    fn base_view(&self, doctype: &str) -> Option<ViewDef> {
        self.base_views.get(doctype).cloned()
    }

    fn scripts(&self) -> Vec<ScriptRecord> {
        self.scripts.values().cloned().collect()
    }

    fn rules(&self) -> Vec<RuleRecord> {
        self.rules
            .iter()
            .map(|(name, expression)| RuleRecord {
                name: name.clone(),
                expression: expression.clone(),
            })
            .collect()
    }

    fn fixtures(&self) -> Vec<FixtureSet> {
        self.fixtures
            .iter()
            .map(|(doctype, documents)| FixtureSet {
                doctype: doctype.clone(),
                documents: documents.clone(),
            })
            .collect()
    }

    fn is_system_managed(&self, kind: &str, item: &str) -> bool {
        self.system_managed.contains(&exclusion_key(kind, item))
    }

    fn is_secret(&self, kind: &str, item: &str) -> bool {
        self.secrets.contains(&exclusion_key(kind, item))
    }
}

impl SiteWriter for FakeSite {
    fn begin(&mut self) -> Result<(), CustomBundleError> {
        self.snapshot = Some(Snapshot {
            customisations: self.customisations.clone(),
            layers: self.layers.clone(),
            scripts: self.scripts.clone(),
            rules: self.rules.clone(),
            fixtures: self.fixtures.clone(),
        });
        Ok(())
    }

    fn commit(&mut self) -> Result<(), CustomBundleError> {
        self.snapshot = None;
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), CustomBundleError> {
        if let Some(snapshot) = self.snapshot.take() {
            self.customisations = snapshot.customisations;
            self.layers = snapshot.layers;
            self.scripts = snapshot.scripts;
            self.rules = snapshot.rules;
            self.fixtures = snapshot.fixtures;
        }
        Ok(())
    }

    fn upsert_custom_field(
        &mut self,
        doctype: &str,
        spec: FieldSpec,
        insert_after: Option<&str>,
    ) -> Result<(), CustomBundleError> {
        self.replace_field(doctype, spec, insert_after);
        Ok(())
    }

    fn remove_custom_field(&mut self, doctype: &str, name: &str) -> Result<(), CustomBundleError> {
        self.customisations.remove_field(doctype, name);
        Ok(())
    }

    fn set_override(
        &mut self,
        doctype: &str,
        field: Option<&str>,
        property: Property,
    ) -> Result<(), CustomBundleError> {
        self.customisations.clear(doctype, field, property.key());
        self.customisations.set(doctype, field, property);
        Ok(())
    }

    fn clear_override(
        &mut self,
        doctype: &str,
        field: Option<&str>,
        key: &str,
    ) -> Result<(), CustomBundleError> {
        self.customisations.clear(doctype, field, key);
        Ok(())
    }

    fn upsert_view_layer(&mut self, layer: ViewLayer) -> Result<(), CustomBundleError> {
        if let Some(existing) = self
            .layers
            .iter_mut()
            .find(|existing| existing.doctype == layer.doctype && existing.kind == layer.kind)
        {
            *existing = layer;
        } else {
            self.layers.push(layer);
        }
        Ok(())
    }

    fn remove_view_layer(
        &mut self,
        doctype: &str,
        kind: &LayerKind,
    ) -> Result<(), CustomBundleError> {
        self.layers
            .retain(|layer| !(layer.doctype == doctype && &layer.kind == kind));
        Ok(())
    }

    fn upsert_script(&mut self, record: ScriptRecord) -> Result<(), CustomBundleError> {
        self.scripts.insert(record.name.clone(), record);
        Ok(())
    }

    fn remove_script(&mut self, name: &str) -> Result<(), CustomBundleError> {
        self.scripts.remove(name);
        Ok(())
    }

    fn upsert_rule(&mut self, name: &str, expression: &str) -> Result<(), CustomBundleError> {
        self.rules.insert(name.to_owned(), expression.to_owned());
        Ok(())
    }

    fn remove_rule(&mut self, name: &str) -> Result<(), CustomBundleError> {
        self.rules.remove(name);
        Ok(())
    }

    fn set_fixtures(
        &mut self,
        doctype: &str,
        documents: Vec<serde_json::Value>,
    ) -> Result<(), CustomBundleError> {
        self.fixtures.insert(doctype.to_owned(), documents);
        Ok(())
    }

    fn remove_fixtures(&mut self, doctype: &str) -> Result<(), CustomBundleError> {
        self.fixtures.remove(doctype);
        Ok(())
    }
}
