//! Item maps and the bridge back to a bundle.
//!
//! A section's **items** are the units a merge works on: the stable key to canonical bytes that
//! [`spot_custom_bundle::Section::items`] returns (a custom field, a property override, a view
//! layer, a script, a rule, a fixture DocType). An unknown section kind has no items, so it is
//! merged at file granularity instead.
//!
//! [`MergedSite`] turns a map of merged items back into the typed values
//! `spot_custom_bundle::Section::capture` reads, so the merged bundle's files are produced by the
//! section's own encoder — never a second encoding.

use std::collections::BTreeMap;

use spot_custom_bundle::{
    Bundle, BundleHeader, CustomBundleError, Files, FixtureSet, RuleRecord, ScriptRecord,
    SectionData, SectionRegistry, SiteReader,
};
use spot_meta::{Customisations, Definition, FieldSpec, Property};
use spot_views::{ViewDef, ViewLayer};

use crate::error::{BundleMergeError, codes};

/// A section's items: a stable key to the item's canonical bytes.
pub(crate) type ItemMap = BTreeMap<String, Vec<u8>>;

/// The `@doctype` marker in an override's item key, as `spot-custom-bundle` writes it.
pub(crate) const DOCTYPE_MARKER: &str = "@doctype";

/// The item map of one section kind in one bundle.
///
/// A known kind is decomposed through its [`spot_custom_bundle::Section`]; an unknown kind is its
/// files keyed by path.
pub(crate) fn item_map(bundle: &Bundle, kind: &str, registry: &SectionRegistry) -> ItemMap {
    let empty = Files::new();
    let files = bundle
        .section(kind)
        .map(|data| &data.files)
        .unwrap_or(&empty);
    match registry.get(kind) {
        Some(section) => section.items(files),
        None => files
            .iter()
            .map(|(name, bytes)| (name.clone(), bytes.clone()))
            .collect(),
    }
}

/// The error for an item whose canonical bytes do not parse back into its typed value.
pub(crate) fn invalid_item(key: &str) -> BundleMergeError {
    BundleMergeError::about(
        codes::INVALID_ITEM,
        key,
        "the item's canonical bytes do not parse back into their typed value",
    )
}

/// A parsed meta item key: a custom field (`doctype.name`) or a property override
/// (`doctype.field.key`, or `doctype.@doctype.key` for a property of the DocType itself).
pub(crate) enum MetaKey {
    /// A custom field.
    Field {
        /// The DocType.
        doctype: String,
        /// The field name.
        name: String,
    },
    /// A property override.
    Override {
        /// The DocType.
        doctype: String,
        /// The field the property is set on, or `None` for the DocType itself.
        field: Option<String>,
        /// The property key.
        property: String,
    },
}

/// Parses a meta item key, or `None` for a malformed one.
pub(crate) fn meta_key(key: &str) -> Option<MetaKey> {
    let mut parts = key.splitn(3, '.');
    let doctype = parts.next()?;
    let second = parts.next()?;
    match parts.next() {
        None => Some(MetaKey::Field {
            doctype: doctype.to_owned(),
            name: second.to_owned(),
        }),
        Some(third) => Some(MetaKey::Override {
            doctype: doctype.to_owned(),
            field: (second != DOCTYPE_MARKER).then(|| second.to_owned()),
            property: third.to_owned(),
        }),
    }
}

/// One custom field's item bytes, the mirror of `spot-custom-bundle`'s private `MetaField`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldItem {
    #[serde(default)]
    insert_after: Option<String>,
    spec: FieldSpec,
}

/// Parses a custom field's item bytes.
fn parse_field(key: &str, bytes: &[u8]) -> Result<FieldItem, BundleMergeError> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid_item(key))?;
    toml::from_str(text).map_err(|_| invalid_item(key))
}

/// A custom field's `insert_after` anchor from its item bytes, or `None` when unparseable.
pub(crate) fn field_anchor(bytes: &[u8]) -> Option<Option<String>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let item: FieldItem = toml::from_str(text).ok()?;
    Some(item.insert_after)
}

/// Parses a view layer's item bytes.
fn parse_view(key: &str, bytes: &[u8]) -> Result<ViewLayer, BundleMergeError> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid_item(key))?;
    ViewLayer::parse(key, text).map_err(|_| invalid_item(key))
}

/// Parses a script's item bytes: `app`, `hook_point`, `hash` lines then the source.
fn parse_script(key: &str, bytes: &[u8]) -> Result<ScriptRecord, BundleMergeError> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid_item(key))?;
    let mut lines = text.splitn(4, '\n');
    let app = lines
        .next()
        .and_then(|line| line.strip_prefix("app = "))
        .ok_or_else(|| invalid_item(key))?
        .to_owned();
    let hook_point = lines
        .next()
        .and_then(|line| line.strip_prefix("hook_point = "))
        .ok_or_else(|| invalid_item(key))?
        .to_owned();
    let hash = lines
        .next()
        .and_then(|line| line.strip_prefix("hash = "))
        .ok_or_else(|| invalid_item(key))?
        .to_owned();
    let source = lines.next().unwrap_or_default().to_owned();
    Ok(ScriptRecord {
        name: key.to_owned(),
        app,
        hook_point,
        source,
        hash,
    })
}

/// A script's hook point from its item bytes, or `None` when unparseable.
pub(crate) fn script_hook(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.lines()
        .nth(1)?
        .strip_prefix("hook_point = ")
        .map(str::to_owned)
}

/// Parses a rule's item bytes (its expression).
fn parse_rule(key: &str, bytes: &[u8]) -> Result<RuleRecord, BundleMergeError> {
    let expression = std::str::from_utf8(bytes)
        .map_err(|_| invalid_item(key))?
        .to_owned();
    Ok(RuleRecord {
        name: key.to_owned(),
        expression,
    })
}

/// Parses a fixtures item's bytes (a JSON array of documents).
fn parse_fixtures(key: &str, bytes: &[u8]) -> Result<FixtureSet, BundleMergeError> {
    let documents: Vec<serde_json::Value> =
        serde_json::from_slice(bytes).map_err(|_| invalid_item(key))?;
    Ok(FixtureSet {
        doctype: key.to_owned(),
        documents,
    })
}

/// Rebuilds the site's customisations from a merged meta item map.
pub(crate) fn parse_meta(
    items: &ItemMap,
    customisations: &mut Customisations,
) -> Result<(), BundleMergeError> {
    for (key, bytes) in items {
        match meta_key(key) {
            Some(MetaKey::Field { doctype, name }) => {
                let item = parse_field(key, bytes)?;
                if item.spec.name != name {
                    return Err(invalid_item(key));
                }
                customisations.add_field(&doctype, item.spec, item.insert_after.as_deref());
            }
            Some(MetaKey::Override {
                doctype,
                field,
                property,
            }) => {
                let value = std::str::from_utf8(bytes).map_err(|_| invalid_item(key))?;
                let parsed = Property::parse(&property, value).map_err(|_| invalid_item(key))?;
                customisations.set(&doctype, field.as_deref(), parsed);
            }
            None => return Err(invalid_item(key)),
        }
    }
    Ok(())
}

/// A site built from a map of merged items, read back by the sections' own capture encoders.
pub(crate) struct MergedSite {
    customisations: Customisations,
    layers: Vec<ViewLayer>,
    scripts: Vec<ScriptRecord>,
    rules: Vec<RuleRecord>,
    fixtures: Vec<FixtureSet>,
}

impl MergedSite {
    /// Parses every known section's merged items into the typed values capture reads.
    pub(crate) fn from_items(clean: &BTreeMap<String, ItemMap>) -> Result<Self, BundleMergeError> {
        let mut customisations = Customisations::new();
        if let Some(items) = clean.get("meta") {
            parse_meta(items, &mut customisations)?;
        }
        let mut layers = Vec::new();
        if let Some(items) = clean.get("views") {
            for (key, bytes) in items {
                layers.push(parse_view(key, bytes)?);
            }
        }
        let mut scripts = Vec::new();
        if let Some(items) = clean.get("scripts") {
            for (key, bytes) in items {
                scripts.push(parse_script(key, bytes)?);
            }
        }
        let mut rules = Vec::new();
        if let Some(items) = clean.get("rules") {
            for (key, bytes) in items {
                rules.push(parse_rule(key, bytes)?);
            }
        }
        let mut fixtures = Vec::new();
        if let Some(items) = clean.get("fixtures") {
            for (key, bytes) in items {
                fixtures.push(parse_fixtures(key, bytes)?);
            }
        }
        Ok(Self {
            customisations,
            layers,
            scripts,
            rules,
            fixtures,
        })
    }

    /// The merged custom fields and property overrides.
    pub(crate) fn customisations(&self) -> &Customisations {
        &self.customisations
    }

    /// The merged view layers.
    pub(crate) fn layers(&self) -> &[ViewLayer] {
        &self.layers
    }

    /// The merged scripts.
    pub(crate) fn scripts(&self) -> &[ScriptRecord] {
        &self.scripts
    }

    /// The merged rules.
    pub(crate) fn rules(&self) -> &[RuleRecord] {
        &self.rules
    }

    /// The merged fixtures.
    pub(crate) fn fixtures(&self) -> &[FixtureSet] {
        &self.fixtures
    }
}

/// Reads a bundle's known sections into a [`MergedSite`], for the upgrade check.
pub(crate) fn bundle_site(
    bundle: &Bundle,
    registry: &SectionRegistry,
) -> Result<MergedSite, BundleMergeError> {
    let mut map: BTreeMap<String, ItemMap> = BTreeMap::new();
    for kind in bundle.sections().keys() {
        if registry.get(kind).is_some() {
            map.insert(kind.clone(), item_map(bundle, kind, registry));
        }
    }
    MergedSite::from_items(&map)
}

impl SiteReader for MergedSite {
    fn installed_apps(&self) -> Vec<(String, semver::Version)> {
        Vec::new()
    }

    fn host_version(&self) -> semver::Version {
        semver::Version::new(0, 0, 0)
    }

    fn definitions(&self) -> Vec<Definition> {
        Vec::new()
    }

    fn customisations(&self) -> Customisations {
        self.customisations.clone()
    }

    fn view_layers(&self) -> Vec<ViewLayer> {
        self.layers.clone()
    }

    fn base_view(&self, _doctype: &str) -> Option<ViewDef> {
        None
    }

    fn scripts(&self) -> Vec<ScriptRecord> {
        self.scripts.clone()
    }

    fn rules(&self) -> Vec<RuleRecord> {
        self.rules.clone()
    }

    fn fixtures(&self) -> Vec<FixtureSet> {
        self.fixtures.clone()
    }

    fn is_system_managed(&self, _kind: &str, _item: &str) -> bool {
        false
    }

    fn is_secret(&self, _kind: &str, _item: &str) -> bool {
        false
    }
}

/// Builds the merged bundle from the cleanly merged items.
///
/// Known sections are encoded by their own capture; an unknown section's files are carried
/// through unchanged. The header is our side's metadata, with the section index recomputed by
/// [`Bundle::pack`].
pub(crate) fn build_bundle(
    header: &BundleHeader,
    clean: &BTreeMap<String, ItemMap>,
    unknown: &BTreeMap<String, Files>,
    limits: &spot_custom_bundle::Limits,
) -> Result<Bundle, BundleMergeError> {
    let registry = SectionRegistry::new();
    let site = MergedSite::from_items(clean)?;
    let mut sections: BTreeMap<String, SectionData> = BTreeMap::new();
    for section in registry.iter() {
        let files = section.capture(&site);
        if !files.is_empty() {
            sections.insert(
                section.kind().to_owned(),
                SectionData {
                    schema: section.schema_version(),
                    files,
                },
            );
        }
    }
    for (kind, files) in unknown {
        if files.is_empty() {
            continue;
        }
        let schema = header
            .sections
            .iter()
            .find(|reference| &reference.kind == kind)
            .map_or(1, |reference| reference.schema);
        sections.insert(
            kind.clone(),
            SectionData {
                schema,
                files: files.clone(),
            },
        );
    }
    Bundle::pack(header.clone(), sections, limits).map_err(pack_error)
}

/// Turns a `spot-custom-bundle` pack failure into this crate's error.
fn pack_error(err: CustomBundleError) -> BundleMergeError {
    BundleMergeError::about(codes::PACK_FAILED, err.item(), err.message())
}
