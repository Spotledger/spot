//! `app new`: a whole project — the packable app under `app/`, plus the README, `.gitignore`
//! and generated test beside it, and optionally a native controller crate and a script.

use spot_template::Context;

use crate::error::Result;
use crate::names::AppName;
use crate::tree::FileTree;

use super::{
    DEFAULT_AUTHOR, DEFAULT_VERSION, add, doctype_context, example_fields, prefix_for, role_for,
};

/// The starter DocType's storage name (a constant, so it is valid for any app name).
pub const STARTER_DOCTYPE: &str = "note";
/// The starter DocType's label.
pub const STARTER_LABEL: &str = "Note";
/// The script `--with-script` adds.
pub const STARTER_SCRIPT: &str = "api";

/// The arguments of `app new`.
#[derive(Clone, Debug)]
pub struct AppSpec {
    /// The app's storage name.
    pub name: AppName,
    /// The human title.
    pub title: String,
    /// The semver version.
    pub version: String,
    /// The author recorded in the README.
    pub author: String,
    /// The licence recorded in the README; `None` records that the author has not chosen one.
    pub licence: Option<String>,
    /// Add the native Rust controller crate skeleton.
    pub with_controller: bool,
    /// Add a TypeScript script skeleton.
    pub with_script: bool,
    /// Add one example field per common kind to the starter DocType.
    pub example_fields: bool,
}

impl AppSpec {
    /// A spec for `name` with the documented defaults (`Spotledger`, no licence chosen,
    /// version `0.1.0`, no optional parts).
    #[must_use]
    pub fn new(name: AppName) -> Self {
        let title = name.as_str().to_owned();
        Self {
            name,
            title,
            version: DEFAULT_VERSION.to_owned(),
            author: DEFAULT_AUTHOR.to_owned(),
            licence: None,
            with_controller: false,
            with_script: false,
            example_fields: false,
        }
    }
}

/// Generates the whole project for `spec`.
///
/// # Errors
/// `scaffold.template` when a template fails to render.
pub fn generate(spec: &AppSpec) -> Result<FileTree> {
    let mut tree = FileTree::new();
    let title = spec.title.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = spec.name.as_str();
    let fields = if spec.example_fields {
        example_fields()
    } else {
        Vec::new()
    };

    let app_ctx = Context::new()
        .insert("name", name)
        .insert("title", title.clone())
        .insert("title_toml", crate::render::toml_escape(&title))
        .insert("version", spec.version.clone())
        .insert("author", spec.author.clone())
        .insert("licence", spec.licence.clone().unwrap_or_default())
        .insert("with_controller", spec.with_controller)
        .insert("with_script", spec.with_script)
        .insert("doctype_name", STARTER_DOCTYPE)
        .insert("doctype_label", STARTER_LABEL)
        .insert("script_name", STARTER_SCRIPT)
        .insert("role", role_for(STARTER_LABEL))
        .insert("prefix", prefix_for(STARTER_DOCTYPE));
    let doctype_ctx = doctype_context(name, STARTER_DOCTYPE, STARTER_LABEL, "main", fields);

    add(&mut tree, "app_manifest", "app/manifest.toml", &app_ctx)?;
    tree.insert("app/app.wasm", crate::templates::COMPONENT.to_vec())?;
    add(
        &mut tree,
        "app_doctype",
        &format!("app/doctypes/{STARTER_DOCTYPE}.toml"),
        &doctype_ctx,
    )?;
    add(
        &mut tree,
        "app_grants",
        "app/permissions/grants.toml",
        &app_ctx,
    )?;
    add(
        &mut tree,
        "app_fixtures_readme",
        "app/fixtures/README.md",
        &app_ctx,
    )?;
    add(&mut tree, "app_readme", "README.md", &app_ctx)?;
    add(&mut tree, "app_gitignore", ".gitignore", &app_ctx)?;
    add(&mut tree, "app_test_load", "tests/load.rs", &app_ctx)?;

    if spec.with_controller {
        add(&mut tree, "app_cargo", "Cargo.toml", &app_ctx)?;
        add(&mut tree, "app_lib", "src/lib.rs", &app_ctx)?;
    }
    if spec.with_controller || spec.with_script {
        add(
            &mut tree,
            "app_controllers",
            "app/controllers.toml",
            &app_ctx,
        )?;
    }
    if spec.with_script {
        let script_ctx = Context::new()
            .insert("app", name)
            .insert("script_name", STARTER_SCRIPT);
        add(
            &mut tree,
            "script_route",
            &format!("app/scripts/{STARTER_SCRIPT}.ts"),
            &script_ctx,
        )?;
        add(
            &mut tree,
            "script_types",
            &format!("app/scripts/types/{name}.d.ts"),
            &script_ctx,
        )?;
    }
    Ok(tree)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_app_has_the_core_files() {
        let tree = generate(&AppSpec::new(AppName::parse("acme").expect("valid"))).expect("render");
        for path in [
            "app/manifest.toml",
            "app/doctypes/note.toml",
            "app/permissions/grants.toml",
            "app/fixtures/README.md",
            "README.md",
            ".gitignore",
            "tests/load.rs",
        ] {
            assert!(tree.get(path).is_some(), "missing {path}");
        }
        assert!(tree.get("app/controllers.toml").is_none());
    }

    #[test]
    fn the_flags_add_their_parts() {
        let mut spec = AppSpec::new(AppName::parse("acme").expect("valid"));
        spec.with_controller = true;
        spec.with_script = true;
        let tree = generate(&spec).expect("render");
        for path in [
            "Cargo.toml",
            "src/lib.rs",
            "app/controllers.toml",
            "app/scripts/api.ts",
            "app/scripts/types/acme.d.ts",
        ] {
            assert!(tree.get(path).is_some(), "missing {path}");
        }
    }
}
