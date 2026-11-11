//! `doctype add`: a DocType definition, a starter grant and a load test, appended to an
//! existing app.

use std::path::Path;

use spot_template::Context;

use crate::error::{Result, ScaffoldError};
use crate::names::{AppName, DocTypeName};
use crate::tree::FileTree;

use super::{app_dir, doctype_context, doctype_name, example_fields, role_for};

/// The rights a starter grant holds.
pub const STARTER_RIGHTS: [&str; 6] = ["read", "write", "create", "delete", "report", "select"];

/// The arguments of `doctype add`.
#[derive(Clone, Debug)]
pub struct DoctypeSpec {
    /// The app the DocType belongs to.
    pub app: AppName,
    /// The human label, such as `Sales Invoice`.
    pub label: String,
    /// An explicit storage name; when absent it is derived from the label.
    pub name: Option<String>,
    /// Add one example field per common kind.
    pub example_fields: bool,
    /// The starter role; when absent, `<label> Manager`.
    pub role: Option<String>,
}

/// Generates the DocType, grant and test files for `spec`, reading `project`'s existing app.
///
/// # Errors
/// `scaffold.not_an_app` when `project/app/manifest.toml` is missing or names another app;
/// `scaffold.duplicate` when the DocType (or its grant) already exists; `scaffold.template`
/// when a template fails to render.
pub fn generate(project: &Path, spec: &DoctypeSpec) -> Result<FileTree> {
    let dir = app_dir(project);
    let (_, manifest) = crate::manifest_edit::read_manifest(&dir)?;
    if manifest.app.name.as_str() != spec.app.as_str() {
        return Err(ScaffoldError::new(
            crate::codes::NOT_AN_APP,
            format!(
                "the app at {} is `{}`, not `{}`",
                dir.display(),
                manifest.app.name.as_str(),
                spec.app.as_str()
            ),
        ));
    }
    let name = doctype_name(spec.name.as_deref(), &spec.label)?;
    let doctype_rel = format!("doctypes/{name}.toml");
    let doctype_path = dir.join(&doctype_rel);
    if doctype_path.exists() {
        return Err(ScaffoldError::duplicate(format!(
            "the DocType `{name}` already exists: app/{doctype_rel}"
        )));
    }

    let mut tree = FileTree::new();
    let fields = if spec.example_fields {
        example_fields()
    } else {
        Vec::new()
    };
    let ctx = doctype_context(
        spec.app.as_str(),
        name.as_str(),
        &spec.label,
        "main",
        fields,
    );
    tree.insert_text(
        &format!("app/{doctype_rel}"),
        &crate::render::render("app_doctype", &ctx)?,
    )?;

    let role = spec.role.clone().unwrap_or_else(|| role_for(&spec.label));
    let grants_rel = "permissions/grants.toml";
    let grants_path = dir.join(grants_rel);
    let grants = match std::fs::read_to_string(&grants_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(ScaffoldError::io(format!(
                "app/{grants_rel} could not be read: {e}"
            )));
        }
    };
    if let Some(line) = crate::manifest_edit::grant_doctype_line(&grants, name.as_str()) {
        return Err(ScaffoldError::duplicate(format!(
            "the DocType `{name}` already has a grant at app/{grants_rel}:{line}"
        )));
    }
    let edited =
        crate::manifest_edit::add_grant_entry(&grants, &role, name.as_str(), &STARTER_RIGHTS)?;
    tree.insert_text(&format!("app/{grants_rel}"), &edited)?;

    let test_ctx = Context::new()
        .insert("name", name.as_str())
        .insert("label", spec.label.clone());
    tree.insert_text(
        &format!("tests/{name}_load.rs"),
        &crate::render::render("doctype_test", &test_ctx)?,
    )?;
    Ok(tree)
}

/// The derived or explicit name of a spec, for a caller that wants to print it.
///
/// # Errors
/// As [`doctype_name`].
pub fn resolved_name(spec: &DoctypeSpec) -> Result<DocTypeName> {
    doctype_name(spec.name.as_deref(), &spec.label)
}
