//! Loading and validating one app directory (ADR-17): `app.toml`, `doctypes/*.toml` and, when
//! present, `permissions/*.toml`. Every problem found is returned together, never only the first.
use crate::{Error, Result};
use spot_meta::Definition;
use std::path::{Path, PathBuf};

/// The reserved name of the platform's own app, embedded in the binary.
pub const PLATFORM: &str = "platform";

/// A validated app directory.
pub struct AppSource {
    /// The manifest's `[app] name`.
    pub name: String,
    /// The manifest's `[app] version`.
    pub version: String,
    /// The directory the app was read from.
    pub dir: PathBuf,
    /// The app's DocType definitions, sorted by file name.
    pub definitions: Vec<Definition>,
}

fn controller_problems(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(controller) = text
        .parse::<toml::Table>()
        .ok()
        .and_then(|t| {
            t.get("app")
                .and_then(toml::Value::as_table)
                .and_then(|a| a.get("controller"))
                .cloned()
        })
        .and_then(|c| c.as_str().map(str::to_owned))
    else {
        return problems;
    };
    match spot_controllers::ControllerRef::parse(&controller) {
        Err(e) => problems.push(format!("app.toml: {e}")),
        Ok(reference) => match reference.source() {
            spot_controllers::ControllerSource::Native => {
                if !crate::natives::is_built_in(reference.value()) {
                    problems.push(format!(
                        "app.toml: controller `{controller}` is not built into this binary"
                    ));
                }
            }
            _ => problems.push(format!(
                "app.toml: controller `{controller}` cannot run in this host (only `native:` \
                 controllers built into the binary are available)"
            )),
        },
    }
    problems
}

/// Removes the keys the manifest parser does not know from a copy of `app.toml`, so the
/// remaining content is validated by `spot-manifest` while host-level keys (the controller
/// reference) are validated here.
fn manifest_text(text: &str) -> String {
    let Ok(mut table) = text.parse::<toml::Table>() else {
        return text.to_owned();
    };
    if let Some(app) = table.get_mut("app").and_then(toml::Value::as_table_mut) {
        app.remove("controller");
    }
    toml::to_string(&table).unwrap_or_else(|_| text.to_owned())
}

/// Reads and validates the app at `dir`. All metadata errors are returned in one [`Error`].
pub fn load(dir: &Path) -> Result<AppSource> {
    let mut errors: Vec<String> = Vec::new();
    let label = dir.display().to_string();
    let mut name = String::new();
    let mut version = String::new();
    match std::fs::read_to_string(dir.join("app.toml")) {
        Err(_) => errors.push(format!("{label}: missing app.toml")),
        Ok(text) => {
            errors.extend(controller_problems(&text));
            match spot_manifest::parse(&manifest_text(&text), &format!("{label}/app.toml")) {
                Ok(manifest) => {
                    name = manifest.app.name.as_str().to_owned();
                    version = manifest.app.version.to_string();
                }
                Err(list) => errors.extend(list.iter().map(|e| format!("{label}/app.toml: {e}"))),
            }
        }
    }
    let mut definitions = Vec::new();
    // An app unpacked from a packed bundle carries a manifest, a component and assets and no
    // DocType files (the bundle format has no place for them): it has an empty `doctypes`
    // directory and an `app.wasm`. Such an extension-only app declares no DocTypes; any other
    // directory without them is still an error.
    let extension_only = dir.join("app.wasm").is_file();
    match std::fs::read_dir(dir.join("doctypes")) {
        Err(_) => errors.push(format!("{label}: missing doctypes directory")),
        Ok(entries) => {
            let mut entries: Vec<_> = entries.filter_map(std::result::Result::ok).collect();
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                match std::fs::read_to_string(&path) {
                    Err(_) => errors.push(format!("cannot read {}", path.display())),
                    Ok(text) => match Definition::parse(&path.display().to_string(), &text) {
                        Ok(d) => definitions.push(d),
                        Err(e) => errors.push(e.to_string()),
                    },
                }
            }
        }
    }
    if definitions.is_empty() && errors.is_empty() && !extension_only {
        errors.push(format!("{label}: the app declares no DocTypes"));
    }
    let permissions = dir.join("permissions");
    if permissions.is_dir() {
        let texts = read_permission_texts(&permissions);
        if let Err(list) = spot_permissions::load(&texts) {
            errors.push(format!("{label}/permissions: {list:?}"));
        }
    }
    if errors.is_empty() {
        Ok(AppSource {
            name,
            version,
            dir: dir.to_path_buf(),
            definitions,
        })
    } else {
        Err(Error(errors.join("\n")))
    }
}

/// The embedded platform app's definitions.
pub fn platform_definitions() -> Result<Vec<Definition>> {
    spot_platform_app::definitions().map_err(|e| Error(e.to_string()))
}

/// The `[app] controller` reference of the app at `dir`, when its manifest names one.
///
/// Read without the manifest parser because `spot-manifest` does not model the host-level
/// controller reference: it stays here, where the native table lives.
pub fn controller_reference(dir: &Path) -> Result<Option<String>> {
    let text = std::fs::read_to_string(dir.join("app.toml"))?;
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| Error(format!("{}/app.toml: {e}", dir.display())))?;
    Ok(table
        .get("app")
        .and_then(toml::Value::as_table)
        .and_then(|app| app.get("controller"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned))
}

/// The parsed manifest of the app at `dir` (the one `spot-controllers` registers steps from).
pub fn manifest(dir: &Path) -> Result<spot_manifest::AppManifest> {
    let text = std::fs::read_to_string(dir.join("app.toml"))?;
    spot_manifest::parse(
        &manifest_text(&text),
        &format!("{}/app.toml", dir.display()),
    )
    .map_err(|errors| {
        Error(
            errors
                .iter()
                .map(|e| format!("{}/app.toml: {e}", dir.display()))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })
}

/// Reads one regular file directly inside `dir`. A missing file, a symlink or anything that
/// is not a regular file reads as empty, so a link can never lead out of the directory and a
/// missing file can only shrink the model, never widen it.
fn read_plain_file(dir: &Path, file: &str) -> String {
    let path = dir.join(file);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => std::fs::read_to_string(path).unwrap_or_default(),
        _ => String::new(),
    }
}

/// The permission texts of an app's `permissions/` directory, shared by the app loader and the
/// site model so the two cannot drift: `grants.toml` followed by every other `*grants.toml`
/// (but `field_grants.toml`) sorted by file name and joined with a newline, plus the single
/// `field_grants.toml` and `groups.toml`. Files with other names are ignored.
pub fn read_permission_texts(dir: &Path) -> spot_permissions::ModelTexts {
    let mut extra: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.ends_with("grants.toml") && name != "grants.toml" && name != "field_grants.toml"
        })
        .collect();
    extra.sort();
    let mut grants = vec![read_plain_file(dir, "grants.toml")];
    grants.extend(extra.iter().map(|name| read_plain_file(dir, name)));
    spot_permissions::ModelTexts {
        grants: grants.join("\n"),
        field_grants: read_plain_file(dir, "field_grants.toml"),
        groups: read_plain_file(dir, "groups.toml"),
        shares: String::new(),
    }
}
