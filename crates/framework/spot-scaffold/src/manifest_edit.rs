//! Editing an existing app's manifest and controllers file: additive, exact, and never a
//! regex over the text.
//!
//! The manifest is parsed first through `spot-manifest` (the crate that owns the format), so
//! an edit is applied only to a manifest the platform accepts. The row is then appended with
//! `toml_edit`, which re-serialises only the table it touches and keeps the author's layout.
//! A conflict names the file and the line of the row that already exists.

use std::path::Path;

use spot_manifest::AppManifest;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::error::{Result, ScaffoldError};

/// The manifest's file name inside the app directory.
pub const MANIFEST_FILE: &str = "manifest.toml";
/// The controllers file's name inside the app directory.
pub const CONTROLLERS_FILE: &str = "controllers.toml";

/// Reads and parses an app manifest.
///
/// # Errors
/// `scaffold.manifest` when the file is missing, unreadable or not a manifest the platform
/// accepts.
pub fn read_manifest(app_dir: &Path) -> Result<(String, AppManifest)> {
    let path = app_dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        ScaffoldError::new(
            crate::codes::NOT_AN_APP,
            format!(
                "{} is not a scaffolded app: {MANIFEST_FILE} could not be read: {e}",
                app_dir.display()
            ),
        )
    })?;
    let manifest = spot_manifest::parse(&text, MANIFEST_FILE).map_err(|errors| {
        let rendered: Vec<String> = errors.iter().map(ToString::to_string).collect();
        ScaffoldError::new(
            crate::codes::MANIFEST,
            format!("{MANIFEST_FILE} does not parse: {}", rendered.join("; ")),
        )
    })?;
    Ok((text, manifest))
}

/// Reads the controllers file, or an empty document when it is absent.
///
/// # Errors
/// `scaffold.manifest` when the file exists but is not valid TOML.
pub fn read_controllers(app_dir: &Path) -> Result<String> {
    match std::fs::read_to_string(app_dir.join(CONTROLLERS_FILE)) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(ScaffoldError::new(
            crate::codes::MANIFEST,
            format!("{CONTROLLERS_FILE} could not be read: {e}"),
        )),
    }
}

/// The 1-based line of a byte offset.
#[must_use]
pub fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1
}

/// The line of the first `[[events]]` row whose `step` is `step`.
#[must_use]
pub fn event_step_line(text: &str, step: &str) -> Option<usize> {
    let doc = text.parse::<DocumentMut>().ok()?;
    let events = doc.get("events")?.as_array_of_tables()?;
    for table in events.iter() {
        if table.get("step").and_then(Item::as_str) == Some(step) {
            return Some(table.span().map_or(1, |r| line_of(text, r.start)));
        }
    }
    None
}

/// The line of the first `[[routes]]` row whose `step` is `step`.
#[must_use]
pub fn route_step_line(text: &str, step: &str) -> Option<usize> {
    let doc = text.parse::<DocumentMut>().ok()?;
    let routes = doc.get("routes")?.as_array_of_tables()?;
    for table in routes.iter() {
        if table.get("step").and_then(Item::as_str) == Some(step) {
            return Some(table.span().map_or(1, |r| line_of(text, r.start)));
        }
    }
    None
}

/// Appends one `[[events]]` row and returns the new manifest text.
///
/// # Errors
/// `scaffold.manifest` when the text is not valid TOML or `events` is not an array of tables.
pub fn add_event(
    text: &str,
    doctype: &str,
    event: &str,
    step: &str,
    priority: i64,
    budget_ms: i64,
) -> Result<String> {
    let mut table = Table::new();
    table.decor_mut().set_prefix("# Added by spot-scaffold.\n");
    table.insert("doctype", value(doctype));
    table.insert("event", value(event));
    table.insert("step", value(step));
    table.insert("priority", value(priority));
    table.insert("budget_ms", value(budget_ms));
    append_row(text, "events", table)
}

/// Appends one `[[routes]]` row and returns the new manifest text.
///
/// # Errors
/// `scaffold.manifest` when the text is not valid TOML or `routes` is not an array of tables.
pub fn add_route(text: &str, path: &str, step: &str) -> Result<String> {
    let mut table = Table::new();
    table.decor_mut().set_prefix("# Added by spot-scaffold.\n");
    table.insert("path", value(path));
    table.insert("step", value(step));
    append_row(text, "routes", table)
}

/// Appends a row to the array-of-tables `key`, creating it when absent.
fn append_row(text: &str, key: &str, table: Table) -> Result<String> {
    let mut doc = text.parse::<DocumentMut>().map_err(|e| {
        ScaffoldError::new(
            crate::codes::MANIFEST,
            format!("{MANIFEST_FILE} is not valid TOML: {e}"),
        )
    })?;
    {
        let root = doc.as_table_mut();
        if !root.contains_key(key) {
            root.insert(key, Item::ArrayOfTables(ArrayOfTables::new()));
        }
        let array = root
            .get_mut(key)
            .and_then(Item::as_array_of_tables_mut)
            .ok_or_else(|| {
                ScaffoldError::new(
                    crate::codes::MANIFEST,
                    format!("{MANIFEST_FILE}: `{key}` is not an array of tables"),
                )
            })?;
        array.push(table);
    }
    Ok(doc.to_string())
}

/// Appends one `"<step>" = "<reference>"` row to the controllers file, creating the
/// `[controllers]` table when absent.
///
/// # Errors
/// `scaffold.manifest` when the file is not valid TOML or `controllers` is not a table.
pub fn add_controller_row(text: &str, step: &str, reference: &str) -> Result<String> {
    let mut out = String::new();
    if text.trim().is_empty() {
        out.push_str("[controllers]\n");
    } else {
        let doc = text.parse::<DocumentMut>().map_err(|e| {
            ScaffoldError::new(
                crate::codes::MANIFEST,
                format!("{CONTROLLERS_FILE} is not valid TOML: {e}"),
            )
        })?;
        if let Some(item) = doc.get("controllers")
            && item.as_table().is_none()
        {
            return Err(ScaffoldError::new(
                crate::codes::MANIFEST,
                format!("{CONTROLLERS_FILE}: `controllers` is not a table"),
            ));
        }
        if doc.get("controllers").is_none() {
            out.push_str("[controllers]\n");
        }
        out.push_str(text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&format!("\"{step}\" = \"{reference}\"\n"));
    Ok(out)
}

/// The line of the first `[[entries]]` row whose `doctype` is `doctype`.
#[must_use]
pub fn grant_doctype_line(text: &str, doctype: &str) -> Option<usize> {
    let doc = text.parse::<DocumentMut>().ok()?;
    let entries = doc.get("entries")?.as_array_of_tables()?;
    for table in entries.iter() {
        if table.get("doctype").and_then(Item::as_str) == Some(doctype) {
            return Some(table.span().map_or(1, |r| line_of(text, r.start)));
        }
    }
    None
}

/// Appends one `[[entries]]` grant row and returns the new text.
///
/// # Errors
/// `scaffold.manifest` when the text is not valid TOML or `entries` is not an array of tables.
pub fn add_grant_entry(text: &str, role: &str, doctype: &str, rights: &[&str]) -> Result<String> {
    let mut rights_array = toml_edit::Array::new();
    for right in rights {
        rights_array.push(*right);
    }
    let mut table = Table::new();
    table.decor_mut().set_prefix("# Added by spot-scaffold.\n");
    table.insert("role", value(role));
    table.insert("doctype", value(doctype));
    table.insert("rights", Item::Value(toml_edit::Value::Array(rights_array)));
    append_row(text, "entries", table)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const BASE: &str = "[app]\nname = \"acme\"\nversion = \"0.1.0\"\n";

    #[test]
    fn a_route_row_appends_and_stays_parseable() {
        let edited = add_route(BASE, "/ping", "acme:ping").expect("edits");
        let manifest = spot_manifest::parse(&edited, "manifest.toml").expect("still a manifest");
        assert_eq!(manifest.routes.len(), 1);
        assert_eq!(manifest.routes[0].path.as_str(), "/ping");
        assert!(edited.contains("[[routes]]"));
    }

    #[test]
    fn a_controller_row_appends() {
        let edited = add_controller_row("", "acme:ping", "ts:scripts/ping.ts").expect("edits");
        assert!(edited.contains("[controllers]"));
        assert!(edited.contains("\"acme:ping\" = \"ts:scripts/ping.ts\""));
    }
}
