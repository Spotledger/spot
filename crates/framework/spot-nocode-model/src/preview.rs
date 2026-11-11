//! Preview: the draft compiled into an **ephemeral** metadata overlay and form layout the
//! client renders with sample data in a sandbox tenant view.
//!
//! A preview persists nothing and changes no schema: it builds the effective metadata in
//! memory, generates the list view and the child grids, and returns them with a sample value
//! per field. [`PreviewMetadata::sandbox`] is always true, so a caller can never mistake a
//! preview for a published change. The caller supplies any time; the crate reads no clock.

use sha2::{Digest, Sha256};
use spot_meta::{Customisations, Definition, FieldType, Registry};

use crate::codes;
use crate::draft::Draft;
use crate::validate::{Element, FriendlyIssue, Severity};
use spot_views::{ViewDef, ViewKind, default_view};

/// One field as the preview shows it, with a sample value.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PreviewField {
    /// The field's storage name.
    pub name: String,
    /// Its label.
    pub label: String,
    /// Its type.
    pub ty: FieldType,
    /// Whether it is required.
    pub required: bool,
    /// A sample value to render.
    pub sample: String,
}

/// A child DocType's preview: its grid view and fields.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChildPreview {
    /// The child's name.
    pub doctype: String,
    /// The child's label.
    pub label: String,
    /// The child's grid view.
    pub grid: ViewDef,
    /// The child's fields.
    pub fields: Vec<PreviewField>,
}

/// The compiled preview of a draft.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PreviewMetadata {
    /// The DocType previewed.
    pub doctype: String,
    /// Its label.
    pub label: String,
    /// What identifies the inputs of this preview: lower-case hex SHA-256 over the canonical
    /// serialisation of every definition the preview was compiled from (the draft's DocTypes
    /// and children, then the site's base definitions sorted by name).
    ///
    /// It is **not** a version number and it never counts anything: two previews of the same
    /// inputs in any order carry the same digest, and any change to a definition, a field or a
    /// child moves it. A client keys its cached preview on it, so a preview built from changed
    /// site definitions is never served from the cache of the old ones.
    pub digest: String,
    /// The form the client renders.
    pub form: ViewDef,
    /// The generated list view.
    pub list: Option<ViewDef>,
    /// The child grids.
    pub children: Vec<ChildPreview>,
    /// The root's fields with sample values.
    pub fields: Vec<PreviewField>,
    /// Whether the preview is a sandbox view (always true: nothing is persisted).
    pub sandbox: bool,
    /// Whether the draft compiles without error.
    pub valid: bool,
    /// Every problem found while compiling.
    pub issues: Vec<FriendlyIssue>,
}

/// Compiles a draft into a preview, resolving links against nothing but the draft itself.
///
/// A draft that links to a DocType the draft does not define (an installed DocType) cannot be
/// fully checked here; use [`preview_in`] with the site's definitions for that. Pure and
/// read-only: the draft is untouched, nothing is written and no schema is changed.
#[must_use]
pub fn preview(draft: &Draft) -> PreviewMetadata {
    preview_with(draft, &[])
}

/// As [`preview`], resolving links against the site's installed definitions.
#[must_use]
pub fn preview_in(draft: &Draft, site: &crate::site::SiteContext) -> PreviewMetadata {
    preview_with(draft, site.definitions())
}

/// As [`preview`], with the base definitions the draft's links may point at.
#[must_use]
pub fn preview_with(draft: &Draft, base: &[Definition]) -> PreviewMetadata {
    // The digest names the inputs, so it is computed from them whether or not they compile:
    // a stale error list is as wrong to cache as a stale form.
    let digest = digest_of(draft, base);
    let definitions = draft_definitions(draft, base);
    let mut issues = Vec::new();
    let (form, list, children, fields, valid) =
        match Registry::build(definitions, &Customisations::new()) {
            Ok(meta) => {
                let form = draft.form_layout.to_view(draft.doctype());
                let list = default_view(&meta, draft.doctype(), ViewKind::List).ok();
                let fields = root_fields(draft);
                let children = draft
                    .children
                    .iter()
                    .map(|child| ChildPreview {
                        doctype: child.doctype_def.name.clone(),
                        label: child.doctype_def.label_or_name().to_owned(),
                        grid: default_view(&meta, &child.doctype_def.name, ViewKind::Grid)
                            .unwrap_or_else(|_| child.form_layout.to_view(&child.doctype_def.name)),
                        fields: fields_of(&child.doctype_def.fields),
                    })
                    .collect();
                // The form view is checked against the metadata; every problem is reported.
                for error in draft.form_layout.validate(draft.doctype(), &meta) {
                    issues.push(FriendlyIssue::new(
                        codes::view(error.kind),
                        error.kind.code(),
                        error.message.clone(),
                        element_of(draft.doctype(), error.field.as_deref()),
                        Severity::Error,
                    ));
                }
                let valid = issues.is_empty();
                (form, list, children, fields, valid)
            }
            Err(errors) => {
                for error in &errors {
                    let doctype = error
                        .doctype
                        .clone()
                        .unwrap_or_else(|| draft.doctype().to_owned());
                    let element = match &error.field {
                        Some(field) => Element::field(&doctype, field),
                        None => Element::doctype(&doctype),
                    };
                    issues.push(FriendlyIssue::new(
                        codes::meta(error.kind),
                        codes::meta_source(error.kind),
                        error.message.clone(),
                        element,
                        Severity::Error,
                    ));
                }
                (
                    draft.form_layout.to_view(draft.doctype()),
                    None,
                    Vec::new(),
                    root_fields(draft),
                    false,
                )
            }
        };
    PreviewMetadata {
        doctype: draft.doctype().to_owned(),
        label: draft.doctype_def.label_or_name().to_owned(),
        digest,
        form,
        list,
        children,
        fields,
        sandbox: true,
        valid,
        issues,
    }
}

/// The digest of a preview: SHA-256 over the definitions it was compiled from.
///
/// The serialisation is canonical: the draft's DocType first, then its children in the draft's
/// order, then the site's base definitions sorted by name, each as its TOML document (the same
/// canonical form a publish digests) closed by a NUL that no TOML text can contain. The site's
/// order therefore does not matter and no two different inputs share a byte stream.
fn digest_of(draft: &Draft, base: &[Definition]) -> String {
    let mut site: Vec<&Definition> = base.iter().filter(|d| d.name != draft.doctype()).collect();
    site.sort_by(|a, b| a.name.cmp(&b.name));
    let mut hasher = Sha256::new();
    hasher.update(b"spot-nocode-model-preview\0");
    for definition in std::iter::once(&draft.doctype_def)
        .chain(draft.children.iter().map(|c| &c.doctype_def))
        .chain(site)
    {
        hasher.update(definition.name.as_bytes());
        hasher.update([0]);
        hasher.update(definition.to_toml().unwrap_or_default().as_bytes());
        hasher.update([0]);
    }
    crate::publish::hex(&hasher.finalize())
}

/// The definitions a draft compiles to (the site's, plus the draft's DocType and children).
fn draft_definitions(draft: &Draft, base: &[Definition]) -> Vec<Definition> {
    let mut definitions: Vec<Definition> = base
        .iter()
        .filter(|d| d.name != draft.doctype())
        .cloned()
        .collect();
    definitions.push(draft.doctype_def.clone());
    definitions.extend(draft.children.iter().map(|c| c.doctype_def.clone()));
    definitions
}

/// The root's fields with sample values.
fn root_fields(draft: &Draft) -> Vec<PreviewField> {
    fields_of(&draft.doctype_def.fields)
}

/// The element of a view error: a field when it names one, else the DocType.
fn element_of(doctype: &str, field: Option<&str>) -> Element {
    match field {
        Some(field) => Element::field(doctype, field),
        None => Element::doctype(doctype),
    }
}

/// Sample fields from a definition's field list.
fn fields_of(fields: &[spot_meta::FieldSpec]) -> Vec<PreviewField> {
    fields
        .iter()
        .map(|field| PreviewField {
            name: field.name.clone(),
            label: field.label_or_name().to_owned(),
            ty: field.ty,
            required: field.required,
            sample: sample_value(field),
        })
        .collect()
}

/// A deterministic sample value for a field, by type. No clock is read: a date sample is a
/// fixed date.
#[must_use]
pub fn sample_value(field: &spot_meta::FieldSpec) -> String {
    match field.ty {
        FieldType::Bool => "true".to_owned(),
        FieldType::Int => "1".to_owned(),
        FieldType::Decimal => "0.00".to_owned(),
        FieldType::Text => "Sample text".to_owned(),
        FieldType::LongText => "A longer sample of text.".to_owned(),
        FieldType::Select => field
            .select_values()
            .first()
            .cloned()
            .unwrap_or_else(|| "Option".to_owned()),
        FieldType::Date => "2026-01-01".to_owned(),
        FieldType::Time => "09:00".to_owned(),
        FieldType::DateTime => "2026-01-01T09:00:00Z".to_owned(),
        FieldType::Link | FieldType::DynamicLink => {
            field.options.clone().unwrap_or_else(|| "sample".to_owned())
        }
        FieldType::Json => "{}".to_owned(),
        FieldType::Bytes => "0 bytes".to_owned(),
        FieldType::Geo => "31.5204, 74.3587".to_owned(),
        FieldType::Range => "2026-01-01 to 2026-12-31".to_owned(),
        FieldType::Table | FieldType::TableMultiselect => String::new(),
        FieldType::Attach => "file".to_owned(),
        FieldType::Secret => "(hidden)".to_owned(),
    }
}
