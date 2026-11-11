//! The usage manifest: the tables, columns and indexes a release's binary can touch.
//!
//! The expand/contract gate needs one fact about the **previous** release: does its code still
//! read or write the object a contract step removes? This crate answers it from a *usage
//! manifest* per release.
//!
//! # Source of the manifest
//!
//! Storage only ever sees the [`Catalog`] compiled from metadata (ADR-045: queries are typed and
//! never hand-built SQL), so a binary's schema registry *is* the set of objects it can touch.
//! [`UsageManifest::from_catalog`] derives the manifest from that registry: no build script, no
//! source scan, no runtime instrumentation, and it cannot drift from the binary. It
//! over-approximates (a declared field the code never touches is still listed), which is the safe
//! direction: a contract ships a little later, never too early. A tighter manifest a build script
//! might emit is accepted too — the format is the same — and
//! [`check_manifest`] proves a checked-in one still covers the registry.
//!
//! # Format
//!
//! ```toml
//! release = "2026.10"
//! objects = ["party", "party.note", "sales_invoice"]
//! ```
//!
//! One `[table]` names a table, `[table.column]` names one column. A table entry covers every
//! column of that table (an over-approximation, as above). The text form is deterministic:
//! objects are sorted, so two manifests that describe the same objects render identically.
//!
//! An index is written `index:table.name` (`objects = ["index:edge.gx_edge_parent_rev"]`). It is
//! a separate kind of object: a table or column entry never covers an index, so naming a table
//! does not make the drop of one of its indexes look "still used" for ever. A manifest without
//! any `index:` entry stays readable and means "this release uses no index the gate has to
//! protect". Because [`UsageManifest::from_catalog`] lists every index of the catalogue,
//! [`check_manifest`] reports `missing-in-manifest` for each index of a manifest written before
//! indexes were listed; the fix is to regenerate it with `from_catalog(..).to_toml()`.
//!
//! # Who writes the manifest
//!
//! The manifest of a release is **generated** from its compiled catalogue by the release build
//! and checked in only as an artefact; nobody edits it by hand. A derived index (such as a
//! traversal index) is part of the catalogue, so it is always listed and cannot be forgotten.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use spot_release_meta::check::ReleaseContext;
use spot_storage_contract::Catalog;

use crate::error::Error;
use crate::object::Object;
use crate::release::ReleaseId;

/// What a release's binary can read or write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageManifest {
    release: ReleaseId,
    objects: BTreeSet<Object>,
}

impl UsageManifest {
    /// A manifest for `release` that touches `objects`; duplicates collapse.
    pub fn new(release: ReleaseId, objects: impl IntoIterator<Item = Object>) -> Self {
        Self {
            release,
            objects: objects.into_iter().collect(),
        }
    }

    /// The release this manifest describes.
    #[must_use]
    pub fn release(&self) -> &ReleaseId {
        &self.release
    }

    /// The objects, in a stable order.
    pub fn objects(&self) -> impl Iterator<Item = &Object> {
        self.objects.iter()
    }

    /// How many objects the manifest names.
    #[must_use]
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Whether the manifest names no object at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Whether the binary touches `object` (a table entry covers its columns).
    #[must_use]
    pub fn uses(&self, object: &Object) -> bool {
        self.objects.iter().any(|entry| entry.covers(object))
    }

    /// The objects `previous` names that this manifest does not: the objects whose code stopped
    /// using them. `self` is the release that stopped using them.
    #[must_use]
    pub fn stopped_using(&self, previous: &UsageManifest) -> Vec<Object> {
        previous
            .objects
            .iter()
            .filter(|object| !self.uses(object))
            .cloned()
            .collect()
    }

    /// A manifest for a release that touches the named DocTypes (tables) and no column in
    /// particular — the level `spot-release-meta`'s patch declarations know.
    pub fn from_doctypes(
        release: ReleaseId,
        doctypes: impl IntoIterator<Item = String>,
    ) -> Result<Self, Error> {
        let mut objects = BTreeSet::new();
        for doctype in doctypes {
            objects.insert(Object::table(&doctype)?);
        }
        Ok(Self { release, objects })
    }

    /// The manifest derived from the storage layer's compiled schema registry: every table,
    /// every column and every index a binary with this catalogue can touch.
    #[must_use]
    pub fn from_catalog(release: ReleaseId, catalog: &Catalog) -> Self {
        let mut objects = BTreeSet::new();
        for entity in catalog.entities() {
            let table = entity.name.as_str();
            let lower = table.to_ascii_lowercase();
            objects.insert(Object {
                table: lower.clone(),
                column: None,
                index: false,
            });
            for field in &entity.fields {
                objects.insert(Object {
                    table: lower.clone(),
                    column: Some(field.name.as_str().to_ascii_lowercase()),
                    index: false,
                });
            }
            // Dropping any index (a unique or primary-key one too) is a contract step.
            for index in &entity.indexes {
                objects.insert(Object {
                    table: lower.clone(),
                    column: Some(index.name.as_str().to_ascii_lowercase()),
                    index: true,
                });
            }
        }
        Self { release, objects }
    }

    /// Parses the TOML form.
    pub fn from_toml(text: &str) -> Result<Self, Error> {
        let file: ManifestFile = toml::from_str(text).map_err(|error| Error::BadManifest {
            message: error.to_string(),
        })?;
        let release = ReleaseId::new(&file.release)?;
        let objects = file
            .objects
            .iter()
            .map(|name| Object::parse(name))
            .collect::<Result<BTreeSet<Object>, Error>>()?;
        Ok(Self { release, objects })
    }

    /// The deterministic TOML form.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let file = ManifestFile {
            release: self.release.as_str().to_owned(),
            objects: self.objects.iter().map(Object::manifest_text).collect(),
        };
        match toml::to_string(&file) {
            Ok(text) => text,
            // The file is a plain string and a string list: serialisation cannot fail, but the
            // crate returns no `Result` here and there is no `unwrap` allowed.
            Err(error) => format!("# could not render the manifest: {error}\n"),
        }
    }
}

/// Why a manifest does not match the schema registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ManifestProblemKind {
    /// The registry holds an object the manifest does not name: the manifest is unsound.
    MissingInManifest,
    /// The manifest names an object the registry does not know: the manifest is stale.
    UnknownToCatalogue,
}

impl ManifestProblemKind {
    /// The stable label, for a report or a CI annotation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingInManifest => "missing-in-manifest",
            Self::UnknownToCatalogue => "unknown-to-catalogue",
        }
    }
}

/// One way a manifest disagrees with the storage schema registry.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ManifestProblem {
    /// What is wrong.
    pub kind: ManifestProblemKind,
    /// The object concerned.
    pub object: Object,
    /// A sentence naming the fix.
    pub detail: String,
}

impl std::fmt::Display for ManifestProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} `{}`: {}",
            self.kind.as_str(),
            self.object.manifest_text(),
            self.detail
        )
    }
}

/// Checks a manifest against the storage layer's compiled schema registry.
///
/// A manifest must name every table, every column and every index of the registry: the binary's catalogue
/// names objects its code can touch, so a manifest that misses one could let a contract ship
/// early. A manifest that names an object the registry does not know is stale. The returned
/// problems are deterministic (sorted by kind, then object), so a report is stable.
#[must_use]
pub fn check_manifest(manifest: &UsageManifest, catalog: &Catalog) -> Vec<ManifestProblem> {
    use ManifestProblemKind::{MissingInManifest, UnknownToCatalogue};

    let mut missing = Vec::new();
    for entity in catalog.entities() {
        let table = entity.name.as_str();
        if let Ok(object) = Object::table(table)
            && !manifest.uses(&object)
        {
            missing.push(ManifestProblem {
                kind: MissingInManifest,
                object,
                detail: "the registry holds this table but the manifest does not name it"
                    .to_owned(),
            });
        }
        for index in &entity.indexes {
            if let Ok(object) = Object::index(table, index.name.as_str())
                && !manifest.uses(&object)
            {
                missing.push(ManifestProblem {
                    kind: MissingInManifest,
                    object,
                    detail: "the registry holds this index but the manifest does not name it \
                             (as `index:table.name`); a manifest written before indexes were \
                             listed is regenerated with `UsageManifest::from_catalog(..).to_toml()`"
                        .to_owned(),
                });
            }
        }
        for field in &entity.fields {
            if let Ok(object) = Object::column(table, field.name.as_str())
                && !manifest.uses(&object)
            {
                missing.push(ManifestProblem {
                    kind: MissingInManifest,
                    object,
                    detail: "the registry holds this column but the manifest does not name it"
                        .to_owned(),
                });
            }
        }
    }

    let mut unknown = Vec::new();
    for object in manifest.objects() {
        if let Some(entity) = catalog
            .entities()
            .find(|entity| entity.name.as_str() == object.table)
        {
            if object.index {
                let known = object
                    .column
                    .as_ref()
                    .is_some_and(|name| entity.indexes.iter().any(|i| i.name.as_str() == name));
                if !known {
                    unknown.push(ManifestProblem {
                        kind: UnknownToCatalogue,
                        object: object.clone(),
                        detail: "the registry does not know this index".to_owned(),
                    });
                }
            } else if let Some(column) = &object.column
                && !entity.fields.iter().any(|f| f.name.as_str() == column)
            {
                unknown.push(ManifestProblem {
                    kind: UnknownToCatalogue,
                    object: object.clone(),
                    detail: "the registry does not know this column".to_owned(),
                });
            }
            continue;
        }
        unknown.push(ManifestProblem {
            kind: UnknownToCatalogue,
            object: object.clone(),
            detail: "the registry does not know this table".to_owned(),
        });
    }

    let mut problems = Vec::new();
    problems.append(&mut missing);
    problems.append(&mut unknown);
    problems.sort();
    problems
}

/// The release-metadata check's context: the objects this release's code stopped using, so
/// `spot_release_meta::check_release` can refuse a destructive patch in the same release.
///
/// `stopped_using` returns the **table names** whose usage vanished between `previous` and
/// `current`, because the release-metadata declarations are per DocType. The DocType registry
/// is not this crate's to know, so every DocType is accepted
/// ([`ReleaseContext::knows_doctype`] is always `true`).
#[derive(Clone, Copy, Debug)]
pub struct StopContext<'a> {
    /// The previous release's usage manifest.
    pub previous: &'a UsageManifest,
    /// The release under check's usage manifest.
    pub current: &'a UsageManifest,
}

impl ReleaseContext for StopContext<'_> {
    fn knows_doctype(&self, _doctype: &str) -> bool {
        true
    }

    fn stopped_using(&self) -> Vec<String> {
        self.current
            .stopped_using(self.previous)
            .into_iter()
            // An index is not a DocType: only a table or column entry stops a DocType.
            .filter(|object| !object.is_index())
            .map(|object| object.table)
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect()
    }
}

/// The on-disk manifest, as TOML carries it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    release: String,
    #[serde(default)]
    objects: Vec<String>,
}
