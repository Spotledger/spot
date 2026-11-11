//! The `meta` section: custom fields and property overrides.
//!
//! One TOML file per DocType. A custom field carries its whole [`FieldSpec`] and its
//! `insert_after`; an override carries the property's **existing** text form (the `key=value`
//! pair [`Property::parse`](spot_meta::Property::parse) already reads), never a second
//! encoding. Fields are sorted by name and overrides by field then key, so the file — and the
//! bundle's digest — do not depend on the order the site happened to record them.
//!
//! # Capture excludes
//!
//! - anything the target marks system-managed or secret;
//! - every override whose value equals the app's own value (a redundant override is a no-op,
//!   so it is not part of the site's delta).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_meta::{Customisations, Definition, FieldSpec, Override, Property, Registry};

use crate::error::{CustomBundleError, codes};
use crate::paths::sanitize;
use crate::registry::{
    Change, ChangeAction, Files, Problem, Risk, Section, SiteReader, SiteWriter,
};

/// The `meta` section.
pub struct MetaSection;

/// One DocType's file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetaFile {
    doctype: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fields: Vec<MetaField>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    overrides: Vec<MetaOverride>,
}

/// One custom field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetaField {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    insert_after: Option<String>,
    spec: FieldSpec,
}

/// One property override, in the existing text form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetaOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    key: String,
    value: String,
}

/// The `@doctype` marker in an item key for a property of the DocType itself.
const DOCTYPE_MARKER: &str = "@doctype";

/// The file name for a DocType.
fn file_name(doctype: &str) -> String {
    format!("{}.toml", sanitize(doctype))
}

/// The item key of a custom field.
fn field_key(doctype: &str, name: &str) -> String {
    format!("{doctype}.{name}")
}

/// The item key of an override.
fn override_key(doctype: &str, field: Option<&str>, key: &str) -> String {
    format!("{doctype}.{}.{key}", field.unwrap_or(DOCTYPE_MARKER))
}

/// The text form of an override's value: the inverse of `Property::parse` for the value part.
///
/// Boolean and numeric properties are written the way `parse` reads them; a lookup or masking
/// spec is written as its TOML (which `parse` reads when the value contains a newline); an
/// empty string clears an optional property.
fn property_text(property: &Property) -> String {
    match property {
        Property::Label(v)
        | Property::TitleField(v)
        | Property::FetchFrom(Some(v))
        | Property::DependsOn(Some(v))
        | Property::MandatoryDependsOn(Some(v))
        | Property::ReadOnlyDependsOn(Some(v))
        | Property::Default(Some(v))
        | Property::Description(Some(v))
        | Property::ProtectionGroup(Some(v))
        | Property::Control(Some(v)) => v.clone(),
        Property::Default(None)
        | Property::Description(None)
        | Property::DependsOn(None)
        | Property::MandatoryDependsOn(None)
        | Property::ReadOnlyDependsOn(None)
        | Property::FetchFrom(None)
        | Property::ProtectionGroup(None)
        | Property::Control(None) => String::new(),
        Property::Required(v)
        | Property::ReadOnly(v)
        | Property::AllowOnPosted(v)
        | Property::Hidden(v)
        | Property::InListView(v)
        | Property::InStandardFilter(v)
        | Property::NoCopy(v) => v.to_string(),
        Property::MaxChars(v) => v.to_string(),
        Property::Permlevel(v) => v.to_string(),
        Property::Options(values) | Property::Classification(values) => values.join("\n"),
        Property::Lookup(spec) => match spec {
            Some(spec) => toml::to_string(spec).unwrap_or_default(),
            None => String::new(),
        },
        Property::Masking(spec) => match spec {
            Some(spec) => toml::to_string(spec).unwrap_or_default(),
            None => String::new(),
        },
    }
}

/// The canonical bytes of one custom field for the diff.
fn field_bytes(insert_after: &Option<String>, spec: &FieldSpec) -> Vec<u8> {
    let field = MetaField {
        insert_after: insert_after.clone(),
        spec: spec.clone(),
    };
    toml::to_string(&field).unwrap_or_default().into_bytes()
}

impl MetaSection {
    /// Parses every file, reporting the first failure per file as a problem.
    fn parse_files(&self, files: &Files) -> Result<Vec<MetaFile>, Vec<Problem>> {
        let mut parsed = Vec::new();
        let mut problems = Vec::new();
        for (path, bytes) in files {
            match std::str::from_utf8(bytes) {
                Ok(text) => match toml::from_str::<MetaFile>(text) {
                    Ok(file) => parsed.push(file),
                    Err(err) => problems.push(Problem {
                        code: codes::INVALID_META.to_owned(),
                        item: path.clone(),
                        message: format!("invalid meta TOML: {}", err.message().trim_end()),
                    }),
                },
                Err(_) => problems.push(Problem {
                    code: codes::INVALID_META.to_owned(),
                    item: path.clone(),
                    message: "the file is not valid UTF-8".to_owned(),
                }),
            }
        }
        if problems.is_empty() {
            Ok(parsed)
        } else {
            Err(problems)
        }
    }

    /// Rebuilds the site's [`Customisations`] from the section's files.
    ///
    /// Returns an error when a file does not parse or an override's text form does not.
    pub(crate) fn customisations(
        &self,
        files: &Files,
    ) -> Result<Customisations, CustomBundleError> {
        let parsed = self.parse_files(files).map_err(|problems| {
            CustomBundleError::about(codes::INVALID_META, "meta", problems[0].message.clone())
        })?;
        let mut customisations = Customisations::new();
        for file in &parsed {
            for field in &file.fields {
                customisations.add_field(
                    &file.doctype,
                    field.spec.clone(),
                    field.insert_after.as_deref(),
                );
            }
            for entry in &file.overrides {
                let property = Property::parse(&entry.key, &entry.value).map_err(|err| {
                    CustomBundleError::about(
                        codes::INVALID_META,
                        override_key(&file.doctype, entry.field.as_deref(), &entry.key),
                        format!("the property text does not parse: {err}"),
                    )
                })?;
                customisations.set(&file.doctype, entry.field.as_deref(), property);
            }
        }
        Ok(customisations)
    }
}

impl Section for MetaSection {
    fn kind(&self) -> &'static str {
        "meta"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn risk(&self, action: ChangeAction) -> Risk {
        match action {
            ChangeAction::Add => Risk::Low,
            ChangeAction::Update => Risk::Medium,
            ChangeAction::Remove => Risk::High,
        }
    }

    fn check(&self, files: &Files) -> Vec<Problem> {
        let parsed = match self.parse_files(files) {
            Ok(parsed) => parsed,
            Err(problems) => return problems,
        };
        let mut problems = Vec::new();
        let mut seen = BTreeMap::new();
        for file in &parsed {
            for entry in &file.overrides {
                if let Err(err) = Property::parse(&entry.key, &entry.value) {
                    problems.push(Problem {
                        code: codes::INVALID_META.to_owned(),
                        item: override_key(&file.doctype, entry.field.as_deref(), &entry.key),
                        message: format!("the property text does not parse: {err}"),
                    });
                }
                let key = override_key(&file.doctype, entry.field.as_deref(), &entry.key);
                if seen.insert(key.clone(), ()).is_some() {
                    problems.push(Problem {
                        code: codes::INVALID_META.to_owned(),
                        item: key,
                        message: "the override appears twice".to_owned(),
                    });
                }
            }
            for field in &file.fields {
                let key = field_key(&file.doctype, &field.spec.name);
                if seen.insert(key.clone(), ()).is_some() {
                    problems.push(Problem {
                        code: codes::INVALID_META.to_owned(),
                        item: key,
                        message: "the custom field appears twice".to_owned(),
                    });
                }
            }
        }
        problems
    }

    fn capture(&self, site: &dyn SiteReader) -> Files {
        let customisations = site.customisations();
        let definitions = site.definitions();
        let base = Registry::build(definitions.clone(), &Customisations::new()).ok();
        let mut by_doctype: BTreeMap<String, MetaFile> = BTreeMap::new();

        for field in customisations.fields() {
            let key = field_key(&field.doctype, &field.spec.name);
            let section = self.kind();
            if site.is_system_managed(section, &key) || site.is_secret(section, &key) {
                continue;
            }
            by_doctype
                .entry(field.doctype.clone())
                .or_insert_with(|| MetaFile {
                    doctype: field.doctype.clone(),
                    fields: Vec::new(),
                    overrides: Vec::new(),
                })
                .fields
                .push(MetaField {
                    insert_after: field.insert_after.clone(),
                    spec: field.spec.clone(),
                });
        }

        for over in customisations.overrides() {
            let key = override_key(&over.doctype, over.field.as_deref(), over.property.key());
            let section = self.kind();
            if site.is_system_managed(section, &key) || site.is_secret(section, &key) {
                continue;
            }
            if redundant(&definitions, &base, over) {
                continue;
            }
            by_doctype
                .entry(over.doctype.clone())
                .or_insert_with(|| MetaFile {
                    doctype: over.doctype.clone(),
                    fields: Vec::new(),
                    overrides: Vec::new(),
                })
                .overrides
                .push(MetaOverride {
                    field: over.field.clone(),
                    key: over.property.key().to_owned(),
                    value: property_text(&over.property),
                });
        }

        let mut files = Files::new();
        for (doctype, mut file) in by_doctype {
            file.fields.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
            file.overrides
                .sort_by(|a, b| (&a.field, &a.key).cmp(&(&b.field, &b.key)));
            if let Ok(text) = toml::to_string_pretty(&file) {
                files.insert(file_name(doctype.trim()), text.into_bytes());
            }
        }
        files
    }

    fn items(&self, files: &Files) -> BTreeMap<String, Vec<u8>> {
        let mut items = BTreeMap::new();
        if let Ok(parsed) = self.parse_files(files) {
            for file in parsed {
                for field in file.fields {
                    items.insert(
                        field_key(&file.doctype, &field.spec.name),
                        field_bytes(&field.insert_after, &field.spec),
                    );
                }
                for entry in file.overrides {
                    items.insert(
                        override_key(&file.doctype, entry.field.as_deref(), &entry.key),
                        entry.value.into_bytes(),
                    );
                }
            }
        }
        items
    }

    fn apply(
        &self,
        change: &Change,
        files: &Files,
        site: &mut dyn SiteWriter,
    ) -> Result<(), CustomBundleError> {
        let mut parts = change.key.splitn(3, '.');
        let doctype = parts.next().unwrap_or_default();
        let second = parts.next().unwrap_or_default();
        match parts.next() {
            // `doctype.field`: a custom field.
            None => match change.action {
                ChangeAction::Remove => site.remove_custom_field(doctype, second),
                _ => {
                    let field = self.find_field(files, &change.key).ok_or_else(|| {
                        CustomBundleError::about(
                            codes::APPLY_FAILED,
                            &change.key,
                            "the custom field is not in the bundle's meta files",
                        )
                    })?;
                    site.upsert_custom_field(doctype, field.spec, field.insert_after.as_deref())
                }
            },
            // `doctype.field.key`: a property override.
            Some(key) => {
                let field = (second != DOCTYPE_MARKER).then_some(second);
                match change.action {
                    ChangeAction::Remove => site.clear_override(doctype, field, key),
                    _ => {
                        let entry = self.find_override(files, &change.key).ok_or_else(|| {
                            CustomBundleError::about(
                                codes::APPLY_FAILED,
                                &change.key,
                                "the override is not in the bundle's meta files",
                            )
                        })?;
                        let property =
                            Property::parse(&entry.key, &entry.value).map_err(|err| {
                                CustomBundleError::about(
                                    codes::APPLY_FAILED,
                                    &change.key,
                                    format!("the property text does not parse: {err}"),
                                )
                            })?;
                        site.set_override(doctype, field, property)
                    }
                }
            }
        }
    }
}

impl MetaSection {
    /// Finds a custom field by item key among the section's files.
    fn find_field(&self, files: &Files, key: &str) -> Option<MetaField> {
        for file in self.parse_files(files).unwrap_or_default() {
            for field in file.fields {
                if field_key(&file.doctype, &field.spec.name) == key {
                    return Some(field);
                }
            }
        }
        None
    }

    /// Finds an override by item key among the section's files.
    fn find_override(&self, files: &Files, key: &str) -> Option<MetaOverride> {
        for file in self.parse_files(files).unwrap_or_default() {
            for entry in file.overrides {
                if override_key(&file.doctype, entry.field.as_deref(), &entry.key) == key {
                    return Some(entry);
                }
            }
        }
        None
    }
}

/// Whether an override changes nothing over the app's own definition, so it is not part of the
/// Rebuilds the site's custom field and override delta from a meta section's files. Used by
/// apply's validation, which rebuilds the metadata before anything is written.
pub(crate) fn customisations_from_files(
    files: &Files,
) -> Result<Customisations, CustomBundleError> {
    MetaSection.customisations(files)
}

/// Whether an override changes nothing over the app's own definition, so it is not part of the
/// site's delta.
fn redundant(
    definitions: &[Definition],
    base: &Option<spot_meta::EffectiveMeta>,
    over: &Override,
) -> bool {
    let Some(base) = base else {
        return false;
    };
    let mut single = Customisations::new();
    single.set(&over.doctype, over.field.as_deref(), over.property.clone());
    let Ok(with) = Registry::build(definitions.to_vec(), &single) else {
        return false;
    };
    match &over.field {
        None => {
            let before = base.definition(&over.doctype);
            let after = with.definition(&over.doctype);
            match (before, after) {
                (Some(before), Some(after)) => {
                    before.label == after.label && before.title_field == after.title_field
                }
                _ => false,
            }
        }
        Some(name) => {
            let field = |meta: &spot_meta::EffectiveMeta| {
                meta.definition(&over.doctype).and_then(|definition| {
                    definition
                        .fields
                        .iter()
                        .find(|field| &field.name == name)
                        .cloned()
                })
            };
            match (field(base), field(&with)) {
                (Some(before), Some(after)) => before == after,
                _ => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_override_text_round_trips_through_parse() {
        let set = Property::Control(Some("radio".to_owned()));
        let text = property_text(&set);
        assert_eq!(text, "radio");
        assert_eq!(Property::parse(set.key(), &text).ok(), Some(set));

        let cleared = Property::Control(None);
        let text = property_text(&cleared);
        assert_eq!(text, "");
        assert_eq!(Property::parse(cleared.key(), &text).ok(), Some(cleared));
    }
}
