//! `controller add`: a native controller skeleton plus the manifest event row and the
//! controllers-file row that point the step at it.

use std::path::Path;

use spot_template::Context;

use crate::error::{Result, ScaffoldError};
use crate::names::{AppName, DocTypeName};
use crate::tree::FileTree;

use super::{app_dir, pascal_case};

/// The hook point the skeleton subscribes to.
pub const HOOK: &str = "validate_save";

/// The arguments of `controller add`.
#[derive(Clone, Debug)]
pub struct ControllerSpec {
    /// The app the controller belongs to.
    pub app: AppName,
    /// The DocType the controller controls.
    pub doctype: DocTypeName,
}

/// Generates the skeleton and the manifest and controllers rows for `spec`.
///
/// # Errors
/// `scaffold.not_an_app` when the project's manifest is missing or names another app;
/// `scaffold.unknown_doctype` when the DocType file is absent or invalid;
/// `scaffold.duplicate` when the step or the skeleton already exists; `scaffold.template`
/// when a template fails to render.
pub fn generate(project: &Path, spec: &ControllerSpec) -> Result<FileTree> {
    let dir = app_dir(project);
    let (manifest_text, manifest) = crate::manifest_edit::read_manifest(&dir)?;
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

    let doctype_rel = format!("doctypes/{}.toml", spec.doctype);
    let doctype_text = std::fs::read_to_string(dir.join(&doctype_rel)).map_err(|e| {
        ScaffoldError::new(
            crate::codes::UNKNOWN_DOCTYPE,
            format!(
                "the app has no DocType `{}`: app/{doctype_rel}: {e}",
                spec.doctype
            ),
        )
    })?;
    let definition = spot_meta::Definition::parse(&doctype_rel, &doctype_text).map_err(|e| {
        ScaffoldError::new(
            crate::codes::UNKNOWN_DOCTYPE,
            format!("the DocType `{}` does not load: {e}", spec.doctype),
        )
    })?;
    let label = definition.label_or_name().to_owned();

    let step = format!("{}:validate_{}", spec.app.as_str(), spec.doctype);
    if let Some(line) = crate::manifest_edit::event_step_line(&manifest_text, &step) {
        return Err(ScaffoldError::duplicate(format!(
            "the step `{step}` already exists at app/manifest.toml:{line}"
        )));
    }
    let skeleton_rel = format!("src/{}_controller.rs", spec.doctype);
    if project.join(&skeleton_rel).exists() {
        return Err(ScaffoldError::duplicate(format!(
            "`{skeleton_rel}` already exists"
        )));
    }

    let mut tree = FileTree::new();
    let edited = crate::manifest_edit::add_event(&manifest_text, &label, HOOK, &step, 0, 200)?;
    tree.insert_text("app/manifest.toml", &edited)?;

    let controllers = crate::manifest_edit::read_controllers(&dir)?;
    let reference = format!("native:{}", spec.app.as_str());
    let edited = crate::manifest_edit::add_controller_row(&controllers, &step, &reference)?;
    tree.insert_text("app/controllers.toml", &edited)?;

    let ctx = Context::new()
        .insert("app", spec.app.as_str())
        .insert("doctype", spec.doctype.as_str())
        .insert("struct_name", pascal_case(spec.doctype.as_str()));
    tree.insert_text(
        &skeleton_rel,
        &crate::render::render("controller_skeleton", &ctx)?,
    )?;
    Ok(tree)
}
