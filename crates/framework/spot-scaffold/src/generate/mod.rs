//! The generators: each takes a spec (and, for an edit, the existing app directory) and
//! returns a [`FileTree`] the caller writes with [`FileTree::write_to_disk`].
//!
//! `app new` is pure (no disk reads). `doctype add`, `controller add` and `script add` read
//! the existing app, refuse a name that already exists (naming the file and line), and edit
//! the manifest through `spot-manifest`/`toml_edit`.

pub mod app;
pub mod controller;
pub mod doctype;
pub mod script;

use std::path::{Path, PathBuf};

use spot_template::{Context, CtxValue};

use crate::names::DocTypeName;

/// The directory, inside a generated project, that holds the packable app.
pub const APP_DIR: &str = "app";

/// The packable app directory of a project.
#[must_use]
pub fn app_dir(project: &Path) -> PathBuf {
    project.join(APP_DIR)
}

/// The default author a scaffold records.
pub const DEFAULT_AUTHOR: &str = "Spotledger";
/// The default version a scaffold records.
pub const DEFAULT_VERSION: &str = "0.1.0";

/// A naming-series prefix derived from a DocType name: the first letter of each `_`-separated
/// part, upper-cased, at most 8 characters.
#[must_use]
pub fn prefix_for(name: &str) -> String {
    let mut out = String::new();
    for part in name.split('_').filter(|p| !p.is_empty()) {
        if let Some(c) = part.chars().next() {
            out.extend(c.to_uppercase());
        }
    }
    if out.is_empty() {
        out.push_str("DOC");
    }
    if out.len() > 8 {
        out.truncate(8);
    }
    out
}

/// A starter role name for a DocType label.
#[must_use]
pub fn role_for(label: &str) -> String {
    format!("{label} Manager")
}

/// A PascalCase identifier for a DocType storage name.
#[must_use]
pub fn pascal_case(name: &str) -> String {
    let mut out = String::new();
    for part in name.split('_').filter(|p| !p.is_empty()) {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() {
        out.push_str("Doc");
    }
    out
}

/// The one example field per common kind, for `--example-fields`.
#[must_use]
pub fn example_fields() -> Vec<CtxValue> {
    const FIELDS: [(&str, &str, &str, &str); 8] = [
        ("is_active", "Is Active", "bool", ""),
        ("quantity", "Quantity", "int", ""),
        ("amount", "Amount", "decimal", "scale = 2\nprecision = 18"),
        ("due_date", "Due Date", "date", ""),
        ("start_time", "Start Time", "time", ""),
        ("starts_at", "Starts At", "datetime", ""),
        ("notes", "Notes", "long_text", ""),
        ("extra", "Extra", "json", ""),
    ];
    FIELDS
        .iter()
        .map(|(name, label, ty, body)| {
            CtxValue::Map(vec![
                ("name".to_owned(), CtxValue::from(*name)),
                ("label".to_owned(), CtxValue::from(*label)),
                ("type".to_owned(), CtxValue::from(*ty)),
                ("body".to_owned(), CtxValue::from(*body)),
            ])
        })
        .collect()
}

/// A DocType name derived from a label, or the explicit name when one is given.
///
/// # Errors
/// `scaffold.invalid_name` when neither the explicit name nor the label yields a valid name.
pub fn doctype_name(explicit: Option<&str>, label: &str) -> crate::error::Result<DocTypeName> {
    match explicit {
        Some(name) => DocTypeName::parse(name),
        None => crate::names::derive_doctype_name(label),
    }
}

/// Adds a rendered template to the tree.
fn add(
    tree: &mut crate::tree::FileTree,
    template: &str,
    path: &str,
    context: &Context,
) -> crate::error::Result<()> {
    let text = crate::render::render(template, context)?;
    tree.insert_text(path, &text)
}

/// The context of the DocType template, shared by `app new`'s starter DocType and
/// `doctype add`.
#[must_use]
pub fn doctype_context(
    app: &str,
    name: &str,
    label: &str,
    module: &str,
    fields: Vec<CtxValue>,
) -> Context {
    Context::new()
        .insert("app", app)
        .insert("name", name)
        .insert("label", label)
        .insert("label_toml", crate::render::toml_escape(label))
        .insert("module", module)
        .insert("prefix", prefix_for(name))
        .insert("fields", CtxValue::List(fields))
}
