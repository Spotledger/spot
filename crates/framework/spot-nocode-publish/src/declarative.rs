//! The declarative payload a no-code app carries inside its `.spotapp` bundle.
//!
//! A no-code app has no component, so the parts that *are* code-shaped (a designed DocType,
//! a process, a report) travel as data under `assets/nocode/declarative.toml`. The bundle's
//! `manifest.toml` stays the ordinary `spot-manifest` document; this file is the app's own
//! declarative body, covered by the bundle digest and therefore by the publisher signature.
//!
//! Site customisations (custom fields, view layers, scripts, rules and fixtures) are **not**
//! duplicated here: they travel in the content-addressed customisation bundle at
//! `assets/nocode/customisation.spotc`, captured by `spot-custom-bundle`.

use serde::{Deserialize, Serialize};
use spot_meta::Definition;
use spot_views::ViewDef;

use crate::draft::AppDraft;
use crate::error::Error;

/// The only declarative payload format this crate writes and the newest it reads.
pub const FORMAT: u32 = 1;

/// The bundle path of the declarative payload.
pub const DECLARATIVE_ASSET: &str = "assets/nocode/declarative.toml";

/// The bundle path of the captured customisation bundle.
pub const CUSTOMISATION_ASSET: &str = "assets/nocode/customisation.spotc";

/// Who published the app, as the declarative payload records it.
///
/// The publisher id is a **reference**: authority comes from the signing certificate, never
/// from these bytes. The licence is a reference too; no licence text is carried.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclarativeMeta {
    /// The app's storage name.
    pub name: String,
    /// Its semantic version, as text.
    pub version: String,
    /// The human title.
    pub title: String,
    /// The publisher id; the certificate must name the same id.
    pub publisher: String,
    /// The tenant the app was designed on.
    pub site: String,
    /// The licence reference, when the publisher states one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licence_ref: Option<String>,
}

/// One process or report definition, carried as opaque declarative text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedSource {
    /// The definition's name.
    pub name: String,
    /// `toml` or `json`; the payload is carried byte for byte.
    pub format: String,
    /// The definition text.
    pub source: String,
}

impl NamedSource {
    /// A TOML definition.
    #[must_use]
    pub fn toml(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            format: "toml".to_owned(),
            source: source.into(),
        }
    }

    /// A JSON definition.
    #[must_use]
    pub fn json(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            format: "json".to_owned(),
            source: source.into(),
        }
    }

    /// A definition whose format is detected from the text: TOML unless it is JSON.
    #[must_use]
    pub fn detected(name: impl Into<String>, source: impl Into<String>) -> Self {
        let source = source.into();
        let format = if toml::from_str::<toml::Value>(&source).is_ok() {
            "toml"
        } else {
            "json"
        };
        Self {
            name: name.into(),
            format: format.to_owned(),
            source,
        }
    }
}

/// One item the app declares, in the order the designer selected it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemRecord {
    /// The kind word (`doctype`, `view`, `script`, `process`, ...).
    pub kind: String,
    /// The item's stable key within its kind.
    pub key: String,
}

/// The declarative body of a no-code app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclarativeApp {
    /// The payload format.
    pub format: u32,
    /// The app identity.
    pub app: DeclarativeMeta,
    /// Newly designed DocTypes, with their fields and child tables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub doctypes: Vec<Definition>,
    /// The designed form views, one per designed DocType that has one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forms: Vec<ViewDef>,
    /// Process definitions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<NamedSource>,
    /// Report definitions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<NamedSource>,
    /// The selected items, sorted by kind then key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ItemRecord>,
}

impl DeclarativeApp {
    /// Builds the payload from the draft and the designed definitions.
    #[must_use]
    pub fn from_draft(draft: &AppDraft, doctypes: Vec<Definition>, forms: Vec<ViewDef>) -> Self {
        let mut items: Vec<ItemRecord> = draft
            .items
            .iter()
            .map(|item| ItemRecord {
                kind: item.kind().to_owned(),
                key: item.key(),
            })
            .collect();
        items.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.key.cmp(&b.key)));
        let mut processes = Vec::new();
        let mut reports = Vec::new();
        for item in &draft.items {
            match item {
                crate::draft::ItemRef::Process { name, source } => {
                    processes.push(NamedSource::detected(name.clone(), source.clone()));
                }
                crate::draft::ItemRef::Report { name, source } => {
                    reports.push(NamedSource::detected(name.clone(), source.clone()));
                }
                _ => {}
            }
        }
        processes.sort_by(|a, b| a.name.cmp(&b.name));
        reports.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            format: FORMAT,
            app: DeclarativeMeta {
                name: draft.name.clone(),
                version: draft.version.to_string(),
                title: draft.title.clone(),
                publisher: draft.publisher.clone(),
                site: draft.site.clone(),
                licence_ref: draft.licence_ref.clone(),
            },
            doctypes,
            forms,
            processes,
            reports,
            items,
        }
    }

    /// The canonical TOML the asset carries.
    ///
    /// # Errors
    /// [`Error::Draft`] when the payload cannot be serialised.
    pub fn to_toml(&self) -> Result<String, Error> {
        toml::to_string_pretty(self).map_err(|error| {
            Error::draft(format!(
                "the declarative payload cannot be written: {error}"
            ))
        })
    }

    /// Parses the asset, refusing a format newer than [`FORMAT`].
    ///
    /// # Errors
    /// [`Error::Draft`] when the payload is malformed or too new.
    pub fn from_toml(text: &str) -> Result<Self, Error> {
        let parsed: Self = toml::from_str(text).map_err(|error| {
            Error::draft(format!(
                "the declarative payload does not parse: {}",
                error.message().trim_end()
            ))
        })?;
        if parsed.format > FORMAT {
            return Err(Error::draft(format!(
                "the declarative payload's format is {}; this reader supports {FORMAT}",
                parsed.format
            )));
        }
        Ok(parsed)
    }
}
