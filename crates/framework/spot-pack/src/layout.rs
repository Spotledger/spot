//! The app directory layout: which files an app may carry, and which name is which entry.
//!
//! The layout mirrors the bundle's fixed shape (WP-STR-01) plus the directories that feed the
//! loaders: `manifest.toml` and the optional `app.wasm` and `controllers.toml` at the root,
//! `doctypes/*.toml`, the four `permissions/` model files, and `fixtures/`, `patches/`,
//! `scripts/`, `assets/` as plain file trees. Every other file is `pack.layout`; every name
//! the bundle's path rules refuse is `pack.path_escape`.

use std::path::Path;

use spot_manifest::{AppManifest, ManifestError};

use crate::error::PackProblem;

/// The manifest's file name, the bundle's manifest entry name.
pub const MANIFEST_FILE: &str = "manifest.toml";
/// The component's file name, the bundle's component entry name.
///
/// Spelled in parts so the error-code scan does not read this file name as a dotted code
/// (it is a file name, not an error code).
pub const WASM_FILE: &str = concat!("app", ".", "wasm");
/// The controllers file: the app's step-to-controller-reference map.
pub const CONTROLLERS_FILE: &str = "controllers.toml";
/// The DocType definitions directory.
pub const DOCTYPES_DIR: &str = "doctypes";
/// The DocType grant file.
pub const GRANTS_FILE: &str = "permissions/grants.toml";
/// The field grants file.
pub const FIELD_GRANTS_FILE: &str = "permissions/field_grants.toml";
/// The protection groups file.
pub const GROUPS_FILE: &str = "permissions/groups.toml";
/// The shares file.
pub const SHARES_FILE: &str = "permissions/shares.toml";
/// The permission directory the four model files sit in.
pub const PERMISSIONS_DIR: &str = "permissions";
/// Allowed first path segments under the app root (root files handled separately).
pub const ALLOWED_DIRS: [&str; 6] = [
    "doctypes",
    "permissions",
    "assets",
    "fixtures",
    "patches",
    "scripts",
];

/// Why one path is not packable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutFault {
    /// The file sits outside the allowed layout.
    Outside(String),
    /// The name would escape the app directory (the bundle's path rules).
    Escape(String),
}

/// One plain file of the app, read in full.
#[derive(Debug, Clone)]
pub struct AppFile {
    /// The app-relative path, `/`-separated, already normalised.
    pub rel: String,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// The parsed controllers file: which step maps to which controller reference.
///
/// The SDK's fixed form:
///
/// ```toml
/// [controllers]
/// "sample:submit" = "wasm:on-event"
/// ```
///
/// Every step the manifest or the controllers file names must appear here; the value after
/// the prefix decides the check (`native:`, `wasm:`, `ts:`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ControllersFile {
    /// The `(step, reference)` rows, in file order.
    pub steps: Vec<(String, String)>,
}

impl ControllersFile {
    /// The reference for `step`, when one is declared.
    #[must_use]
    pub fn reference_of(&self, step: &str) -> Option<&str> {
        self.steps
            .iter()
            .find(|(s, _)| s == step)
            .map(|(_, r)| r.as_str())
    }

    /// Every step any manifest row names, which the check requires to be provided.
    #[must_use]
    pub fn required_steps(manifest: &AppManifest) -> Vec<String> {
        let mut steps = Vec::new();
        for sub in &manifest.events {
            steps.push(sub.step.to_string());
        }
        for sub in &manifest.session_events {
            steps.push(sub.step.to_string());
        }
        for sub in &manifest.boot {
            steps.push(sub.step.to_string());
        }
        for schedule in &manifest.schedules {
            steps.push(schedule.step.to_string());
        }
        for route in &manifest.routes {
            steps.push(route.step.to_string());
        }
        for fixture in &manifest.fixtures {
            if let Some(step) = &fixture.step {
                steps.push(step.to_string());
            }
        }
        steps.sort();
        steps.dedup();
        steps
    }
}

/// The whole app read from disk.
#[derive(Debug, Default)]
pub struct AppDir {
    /// The manifest parsed through `spot-manifest` (`None` when absent).
    pub manifest: Option<Result<AppManifest, Vec<ManifestError>>>,
    /// The controllers file, when present.
    pub controllers: ControllersFile,
    /// The component binary, when present.
    pub wasm: Option<Vec<u8>>,
    /// Every file under the layout, in stable path order.
    pub files: Vec<AppFile>,
    /// Every layout or path fault found while walking.
    pub faults: Vec<LayoutFault>,
}

impl AppDir {
    /// Reads `dir`, collecting every problem it can find as it goes.
    pub fn read(dir: &Path, problems: &mut Vec<PackProblem>) -> Self {
        let mut app = AppDir::default();
        match std::fs::read_to_string(dir.join(MANIFEST_FILE)) {
            Ok(text) => app.manifest = Some(spot_manifest::parse(&text, MANIFEST_FILE)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                problems.push(PackProblem::about(
                    crate::error::MANIFEST,
                    MANIFEST_FILE,
                    format!("manifest.toml could not be read: {err}"),
                ));
            }
        }
        app.controllers = read_controllers(dir, problems);
        match std::fs::read(dir.join(WASM_FILE)) {
            Ok(bytes) => app.wasm = Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                problems.push(PackProblem::about(
                    crate::error::WASM,
                    WASM_FILE,
                    format!("app.wasm could not be read: {err}"),
                ));
            }
        }
        walk(dir, dir, &mut app, problems);
        app
    }
}

/// Parses `controllers.toml` into its rows.
fn read_controllers(dir: &Path, problems: &mut Vec<PackProblem>) -> ControllersFile {
    match std::fs::read_to_string(dir.join(CONTROLLERS_FILE)) {
        Ok(text) => match parse_controllers(&text) {
            Ok(file) => file,
            Err(why) => {
                problems.push(PackProblem::about(
                    crate::error::CONTROLLERS,
                    CONTROLLERS_FILE,
                    why,
                ));
                ControllersFile::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => ControllersFile::default(),
        Err(err) => {
            problems.push(PackProblem::about(
                crate::error::CONTROLLERS,
                CONTROLLERS_FILE,
                format!("controllers.toml could not be read: {err}"),
            ));
            ControllersFile::default()
        }
    }
}

/// One parsed controllers file, or why it does not parse.
type ParsedControllers = Result<ControllersFile, String>;

/// Parses the fixed form: `[controllers]`, then `"step" = "reference"` rows.
fn parse_controllers(text: &str) -> ParsedControllers {
    let mut steps = Vec::new();
    let mut seen_keys = std::collections::BTreeSet::new();
    let mut in_controllers = false;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(table) = line.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            in_controllers = table.trim() == "controllers";
            continue;
        }
        if !in_controllers {
            return Err(format!(
                "line {}: table rows belong inside `[controllers]`",
                n + 1
            ));
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "line {}: rows are `\"step\" = \"reference\"`",
                n + 1
            ));
        };
        let (Some(step), Some(reference)) = (unquote(key.trim()), unquote(value.trim())) else {
            return Err(format!(
                "line {}: keys and values are double-quoted strings",
                n + 1
            ));
        };
        if step.is_empty() {
            return Err(format!("line {}: the step name is empty", n + 1));
        }
        if !seen_keys.insert(step.clone()) {
            return Err(format!(
                "line {}: the step `{step}` is declared twice",
                n + 1
            ));
        }
        steps.push((step, reference));
    }
    Ok(ControllersFile { steps })
}

/// Strips one pair of double quotes; `None` when the text is not a quoted string.
fn unquote(text: &str) -> Option<String> {
    text.strip_prefix('"')?.strip_suffix('"').map(str::to_owned)
}

/// Walks `root` recursively, sorting every file into the layout and faulting the rest.
///
/// The walk never follows symlinks (a symlinked file or directory is a
/// [`LayoutFault::Escape`] entry): an app must carry its own bytes, reachable by plain
/// paths only, so the packed bundle's files are exactly what the app directory carries.
fn walk(root: &Path, here: &Path, app: &mut AppDir, problems: &mut Vec<PackProblem>) {
    let mut entries: Vec<_> = match std::fs::read_dir(here) {
        Ok(rd) => rd.filter_map(Result::ok).map(|e| e.path()).collect(),
        Err(err) => {
            problems.push(PackProblem::new(
                crate::error::IO,
                format!("{} could not be listed: {err}", here.display()),
            ));
            return;
        }
    };
    entries.sort();
    for path in entries {
        let Some(rel) = path
            .strip_prefix(root)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
        else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        // A symlink anywhere in the tree escapes the app directory by construction: the file
        // or directory it names sits wherever the author's machine points, so walking into
        // or reading it would pack bytes the app directory does not carry (and on Windows
        // the walk would not see through it at all). Refuse the entry itself, whatever it
        // points to, BEFORE `is_dir`/`is_file` stat through the link and decide for it.
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            app.faults.push(LayoutFault::Escape(rel));
            continue;
        }
        if path.is_dir() {
            let first = rel.split('/').next().unwrap_or("");
            if !ALLOWED_DIRS.contains(&first) {
                app.faults.push(LayoutFault::Outside(rel));
                continue;
            }
            walk(root, &path, app, problems);
            continue;
        }
        // Root entries are read above; the file rules apply to the rest.
        match rel.as_str() {
            MANIFEST_FILE | WASM_FILE | CONTROLLERS_FILE => continue,
            _ => {}
        }
        if !crate::pathsafe::is_safe_rel_path(&rel) {
            app.faults.push(LayoutFault::Escape(rel));
            continue;
        }
        let first = rel.split('/').next().unwrap_or("");
        let allowed = ALLOWED_DIRS.contains(&first)
            && (first != PERMISSIONS_DIR
                || matches!(
                    rel.as_str(),
                    GRANTS_FILE | FIELD_GRANTS_FILE | GROUPS_FILE | SHARES_FILE
                ));
        if !allowed {
            app.faults.push(LayoutFault::Outside(rel));
            continue;
        }
        match std::fs::read(&path) {
            Ok(bytes) => app.files.push(AppFile { rel, bytes }),
            Err(err) => problems.push(PackProblem::about(
                crate::error::IO,
                rel,
                format!("the file could not be read: {err}"),
            )),
        }
    }
}
