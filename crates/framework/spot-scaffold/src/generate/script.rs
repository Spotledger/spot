//! `script add`: a TypeScript route handler, its types file, and the manifest route row and
//! controllers-file row that point the step at it.

use std::path::Path;

use spot_template::Context;

use crate::error::{Result, ScaffoldError};
use crate::names::{AppName, NameKind};
use crate::tree::FileTree;

use super::app_dir;

/// The arguments of `script add`.
#[derive(Clone, Debug)]
pub struct ScriptSpec {
    /// The app the script belongs to.
    pub app: AppName,
    /// The script's storage name (also its route and step).
    pub name: String,
}

/// Generates the script, types and rows for `spec`.
///
/// # Errors
/// `scaffold.invalid_name` when the script name is not a storage name; `scaffold.not_an_app`
/// when the project's manifest is missing or names another app; `scaffold.duplicate` when the
/// step or the script file already exists; `scaffold.template` when a template fails.
pub fn generate(project: &Path, spec: &ScriptSpec) -> Result<FileTree> {
    let name = crate::names::validate(NameKind::Script, &spec.name)?;
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

    let step = format!("{}:{name}", spec.app.as_str());
    if let Some(line) = crate::manifest_edit::route_step_line(&manifest_text, &step) {
        return Err(ScaffoldError::duplicate(format!(
            "the step `{step}` already exists at app/manifest.toml:{line}"
        )));
    }
    let script_rel = format!("scripts/{name}.ts");
    if dir.join(&script_rel).exists() {
        return Err(ScaffoldError::duplicate(format!(
            "`app/{script_rel}` already exists"
        )));
    }

    let mut tree = FileTree::new();
    let route = format!("/{name}");
    let edited = crate::manifest_edit::add_route(&manifest_text, &route, &step)?;
    tree.insert_text("app/manifest.toml", &edited)?;

    let controllers = crate::manifest_edit::read_controllers(&dir)?;
    let reference = format!("ts:{script_rel}");
    let edited = crate::manifest_edit::add_controller_row(&controllers, &step, &reference)?;
    tree.insert_text("app/controllers.toml", &edited)?;

    let ctx = Context::new()
        .insert("app", spec.app.as_str())
        .insert("script_name", name.clone());
    tree.insert_text(
        &format!("app/{script_rel}"),
        &crate::render::render("script_route", &ctx)?,
    )?;
    tree.insert_text(
        &format!("app/scripts/types/{}.d.ts", spec.app.as_str()),
        &crate::render::render("script_types", &ctx)?,
    )?;
    Ok(tree)
}
