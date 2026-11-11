//! Business-friendly validation: the draft is compiled to the effective metadata the platform
//! runs, and every problem the registry or the view layer reports is turned into a plain-language
//! message with a fix suggestion and the element path the designer must point at.
//!
//! The registry's own message and its stable code (`meta.*`, `view.*`) are **kept** on every
//! [`FriendlyIssue`] (`source_code`, `source_message`), so a support engineer still sees the
//! underlying cause while the designer sees "This field needs a list of choices" and "Add at
//! least one choice". The plain-language text comes from the message catalogue
//! (`crates/framework/spot-errors/catalogue/nocode_model.toml`), keyed by the friendly
//! `nocode_model.*` code.

use spot_meta::{Customisations, Definition, Registry};
use spot_views::ViewError;

use crate::codes;
use crate::draft::{
    Draft, MAX_FIELDS, is_protected_doctype, is_reserved_field, looks_like_script, looks_like_sql,
};
use crate::site::SiteContext;

/// How serious an issue is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The draft cannot be published until it is fixed.
    Error,
    /// The draft can be published, but the designer should look.
    Warning,
}

/// The element an issue is about, so the client can select it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Element {
    /// The DocType (or child DocType) the element belongs to.
    pub doctype: String,
    /// The field, when the issue is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// The layout section, when the issue is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
}

impl Element {
    /// An element at a DocType.
    #[must_use]
    pub fn doctype(doctype: &str) -> Self {
        Self {
            doctype: doctype.to_owned(),
            field: None,
            section: None,
        }
    }

    /// An element at a field.
    #[must_use]
    pub fn field(doctype: &str, field: &str) -> Self {
        Self {
            doctype: doctype.to_owned(),
            field: Some(field.to_owned()),
            section: None,
        }
    }

    /// An element at a layout section.
    #[must_use]
    pub fn section(doctype: &str, section: &str) -> Self {
        Self {
            doctype: doctype.to_owned(),
            field: None,
            section: Some(section.to_owned()),
        }
    }

    /// The element's path: `doctype`, `doctype.field` or `doctype#section`.
    #[must_use]
    pub fn path(&self) -> String {
        match (&self.field, &self.section) {
            (Some(field), _) => format!("{}.{field}", self.doctype),
            (None, Some(section)) => format!("{}#{section}", self.doctype),
            (None, None) => self.doctype.clone(),
        }
    }
}

/// One problem a designer sees: the friendly code, the plain-language message, the fix and the
/// element path, with the registry's own code and message kept beside them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FriendlyIssue {
    /// The stable friendly code (`nocode_model.*`).
    pub code: String,
    /// The underlying code (`meta.*`, `view.*` or `nocode_model.*`).
    pub source_code: String,
    /// The underlying message, kept for a support engineer.
    pub source_message: String,
    /// The element the issue is about.
    pub element: Element,
    /// The plain-language message.
    pub message: String,
    /// What to do about it.
    pub suggestion: String,
    /// How serious it is.
    pub severity: Severity,
}

impl FriendlyIssue {
    /// Builds an issue from a friendly code and the underlying cause. The message and suggestion
    /// come from the message catalogue; a code the catalogue does not know falls back to the
    /// underlying message so nothing is ever swallowed.
    #[must_use]
    pub fn new(
        code: &'static str,
        source_code: impl Into<String>,
        source_message: impl Into<String>,
        element: Element,
        severity: Severity,
    ) -> Self {
        let source_message = source_message.into();
        let (message, suggestion) = match spot_errors::Catalogue::load().lookup(code) {
            Some(entry) => (entry.summary.clone(), entry.remedy.clone()),
            None => (source_message.clone(), String::new()),
        };
        Self {
            code: code.to_owned(),
            source_code: source_code.into(),
            source_message,
            element,
            message,
            suggestion,
            severity,
        }
    }

    /// Builds an issue from an [`OpError`](crate::OpError): its code, message and element path.
    #[must_use]
    pub fn from_op_error(error: &crate::draft::OpError, doctype: &str, severity: Severity) -> Self {
        let element = match error.path() {
            Some(path) => match path.rsplit_once('.') {
                Some((owner, field)) => Element::field(owner, field),
                None => Element::doctype(doctype),
            },
            None => Element::doctype(doctype),
        };
        Self::new(
            error.code(),
            error.code(),
            error.to_string(),
            element,
            severity,
        )
    }

    /// The element path, for a client that wants a flat string.
    #[must_use]
    pub fn path(&self) -> String {
        self.element.path()
    }
}

/// The result of validating a draft: every issue, in the order they were found.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ValidationReport {
    /// Every issue.
    pub issues: Vec<FriendlyIssue>,
}

impl ValidationReport {
    /// Whether the draft may be published (no error-severity issue).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        !self.issues.iter().any(|i| i.severity == Severity::Error)
    }

    /// The error-severity issues.
    pub fn errors(&self) -> impl Iterator<Item = &FriendlyIssue> {
        self.issues.iter().filter(|i| i.severity == Severity::Error)
    }

    /// The warning-severity issues.
    pub fn warnings(&self) -> impl Iterator<Item = &FriendlyIssue> {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Warning)
    }

    /// The friendly codes, in order.
    #[must_use]
    pub fn codes(&self) -> Vec<String> {
        self.issues.iter().map(|i| i.code.clone()).collect()
    }
}

/// Validates a draft against the site, reporting every problem at once.
#[must_use]
pub fn validate(draft: &Draft, site: &SiteContext) -> ValidationReport {
    let mut report = ValidationReport::default();
    guard_rails(draft, site, &mut report);
    let definitions = definitions_for(draft, site);
    let meta = match Registry::build(definitions, &Customisations::new()) {
        Ok(meta) => Some(meta),
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
                report.issues.push(FriendlyIssue::new(
                    codes::meta(error.kind),
                    codes::meta_source(error.kind),
                    error.message.clone(),
                    element,
                    Severity::Error,
                ));
            }
            None
        }
    };
    if let Some(meta) = &meta {
        validate_layouts(draft, meta, &mut report);
    }
    report
}

/// Checks the guard rails that do not need the registry.
fn guard_rails(draft: &Draft, site: &SiteContext, report: &mut ValidationReport) {
    let doctype = draft.doctype();
    if site.is_protected(doctype) || is_protected_doctype(doctype) {
        report.issues.push(FriendlyIssue::new(
            codes::PROTECTED_DOCTYPE,
            codes::PROTECTED_DOCTYPE,
            format!("`{doctype}` is a protected system DocType"),
            Element::doctype(doctype),
            Severity::Error,
        ));
    }
    if draft.doctype_def.fields.len() > MAX_FIELDS {
        report.issues.push(FriendlyIssue::new(
            codes::MAX_FIELDS,
            codes::MAX_FIELDS,
            format!(
                "`{doctype}` holds {} fields",
                draft.doctype_def.fields.len()
            ),
            Element::doctype(doctype),
            Severity::Error,
        ));
    }
    if let Some(pattern) = &draft.doctype_def.naming
        && !crate::ops::naming_is_valid(pattern)
    {
        report.issues.push(FriendlyIssue::new(
            codes::BAD_NAMING,
            codes::BAD_NAMING,
            format!("`{pattern}` is not a naming pattern"),
            Element::doctype(doctype),
            Severity::Error,
        ));
    }
    check_fields(&draft.doctype_def, doctype, report);
    for child in &draft.children {
        let child_name = &child.doctype_def.name;
        check_fields(&child.doctype_def, child_name, report);
    }
    check_existing(draft, site, report);
}

/// Checks the fields of one definition for reserved names and forbidden content.
fn check_fields(definition: &Definition, doctype: &str, report: &mut ValidationReport) {
    if definition.fields.len() > MAX_FIELDS {
        report.issues.push(FriendlyIssue::new(
            codes::MAX_FIELDS,
            codes::MAX_FIELDS,
            format!("`{doctype}` holds {} fields", definition.fields.len()),
            Element::doctype(doctype),
            Severity::Error,
        ));
    }
    for field in &definition.fields {
        if is_reserved_field(&field.name) {
            report.issues.push(FriendlyIssue::new(
                codes::RESERVED_NAME,
                codes::RESERVED_NAME,
                format!("`{}` is a reserved name", field.name),
                Element::field(doctype, &field.name),
                Severity::Error,
            ));
        }
        for (what, text) in [
            ("default", field.default.as_deref()),
            ("options", field.options.as_deref()),
            ("depends_on", field.depends_on.as_deref()),
        ] {
            let Some(text) = text else { continue };
            if looks_like_script(text) {
                report.issues.push(FriendlyIssue::new(
                    codes::NO_SCRIPT,
                    codes::NO_SCRIPT,
                    format!("`{}` {what} looks like script", field.name),
                    Element::field(doctype, &field.name),
                    Severity::Error,
                ));
            }
            if looks_like_sql(text) {
                report.issues.push(FriendlyIssue::new(
                    codes::NO_SQL,
                    codes::NO_SQL,
                    format!("`{}` {what} looks like SQL", field.name),
                    Element::field(doctype, &field.name),
                    Severity::Error,
                ));
            }
        }
    }
}

/// Checks a draft that edits an existing DocType: new fields must be custom fields, and an app
/// field may not be dropped.
fn check_existing(draft: &Draft, site: &SiteContext, report: &mut ValidationReport) {
    let doctype = draft.doctype();
    let Some(existing) = site.definition(doctype) else {
        return;
    };
    for field in &draft.doctype_def.fields {
        let is_new = !existing.fields.iter().any(|f| f.name == field.name);
        if is_new && !field.name.starts_with(spot_meta::CUSTOM_PREFIX) {
            report.issues.push(FriendlyIssue::new(
                codes::EXISTING_NEEDS_PREFIX,
                codes::EXISTING_NEEDS_PREFIX,
                format!(
                    "`{}` is a new field on an existing DocType and is not a custom field",
                    field.name
                ),
                Element::field(doctype, &field.name),
                Severity::Error,
            ));
        }
    }
    for field in &existing.fields {
        let removed = !draft
            .doctype_def
            .fields
            .iter()
            .any(|f| f.name == field.name);
        if removed && !field.name.starts_with(spot_meta::CUSTOM_PREFIX) {
            report.issues.push(FriendlyIssue::new(
                codes::NOT_CUSTOM_FIELD,
                codes::NOT_CUSTOM_FIELD,
                format!("`{}` is an app field and cannot be removed", field.name),
                Element::field(doctype, &field.name),
                Severity::Error,
            ));
        }
    }
}

/// Validates the root and child layouts over the built metadata.
fn validate_layouts(draft: &Draft, meta: &spot_meta::EffectiveMeta, report: &mut ValidationReport) {
    push_view_errors(draft.form_layout.validate(draft.doctype(), meta), report);
    for child in &draft.children {
        push_view_errors(
            child.form_layout.validate(&child.doctype_def.name, meta),
            report,
        );
    }
}

/// Turns view errors into friendly issues.
fn push_view_errors(errors: Vec<ViewError>, report: &mut ValidationReport) {
    for error in errors {
        let doctype = error.doctype.clone().unwrap_or_default();
        let element = match &error.field {
            Some(field) => Element::field(&doctype, field),
            None => Element::doctype(&doctype),
        };
        report.issues.push(FriendlyIssue::new(
            codes::view(error.kind),
            error.kind.code(),
            error.message.clone(),
            element,
            Severity::Error,
        ));
    }
}

/// The definitions the draft compiles to: the site's, with the draft's DocType replaced and its
/// children added.
#[must_use]
pub fn definitions_for(draft: &Draft, site: &SiteContext) -> Vec<Definition> {
    let mut definitions: Vec<Definition> = site
        .definitions()
        .iter()
        .filter(|d| d.name != draft.doctype())
        .cloned()
        .collect();
    definitions.push(draft.doctype_def.clone());
    for child in &draft.children {
        definitions.push(child.doctype_def.clone());
    }
    definitions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draft::DraftOp;
    use spot_core::SiteId;

    fn site() -> SiteContext {
        let site = SiteId::new("acme.example")
            .unwrap_or_else(|e| panic!("`acme.example` is a site id: {e}"));
        SiteContext::new(site)
    }

    #[test]
    fn a_valid_draft_reports_nothing() {
        let mut draft = Draft::new("d1", "ann", "vehicle", "Vehicle");
        draft = crate::draft::apply_op(
            &draft,
            &DraftOp::AddField {
                child: None,
                name: "plate".to_owned(),
                label: None,
                ty: spot_meta::FieldType::Text,
                insert_after: None,
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let report = validate(&draft, &site());
        assert!(report.is_ok(), "{:?}", report.codes());
    }

    #[test]
    fn a_reserved_field_name_is_reported_in_plain_language() {
        let mut draft = Draft::new("d1", "ann", "vehicle", "Vehicle");
        // `id` is a system column.
        draft
            .doctype_def
            .fields
            .push(spot_meta::FieldSpec::new("id", spot_meta::FieldType::Int));
        let report = validate(&draft, &site());
        let issue = report
            .issues
            .iter()
            .find(|i| i.code == codes::RESERVED_NAME)
            .cloned();
        assert!(issue.is_some());
        let issue = issue.unwrap_or_else(|| unreachable!());
        assert_eq!(issue.path(), "vehicle.id");
        assert!(!issue.message.is_empty());
        assert!(!issue.suggestion.is_empty());
    }
}
