//! Checking an app directory end to end: every problem at once, each with its file.
//!
//! [`check_app`] reads the app the way the installer will: the manifest through
//! `spot-manifest`, every DocType through `spot-meta` (which runs the metadata rules and the
//! security lints), the permission model through `spot-permissions`, the component and its
//! imports against the manifest's capabilities exactly as `spot-bundle`'s privilege check
//! runs, and every file name through the bundle's path rules. A check that finds nothing can
//! be packed.

use std::collections::BTreeSet;
use std::path::Path;

use spot_bundle::Limits;
use spot_manifest::AppManifest;
use spot_permissions::ModelTexts;

use crate::error::PackProblem;
use crate::layout::{
    self, AppDir, CONTROLLERS_FILE, ControllersFile, LayoutFault, MANIFEST_FILE, WASM_FILE,
};
use crate::wasm_component;

/// What one `native:`, `wasm:` or `ts:` controller reference means.
///
/// The reference is the value the controllers file maps a step to (the controller factory of
/// WP-DOC-10 consumes the same three forms). The prefix decides the check:
///
/// - `native:<name>` — the name must appear in the optional controller name list the host
///   compiled in. Packing needs no Rust code, so the list is a parameter: `None` skips the
///   check, `Some(names)` refuses a name outside it.
/// - `wasm:<export>` — the app's `app.wasm` must parse and carry an export with this name.
/// - `ts:<path>` — the file must exist and transpile with no diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerRef {
    /// The reference as written, including the prefix.
    pub reference: String,
    /// The kind the prefix names.
    pub kind: ControllerKind,
}

/// The kind of a [`ControllerRef`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControllerKind {
    /// `native:<name>`: looked up in the host's controller table.
    Native,
    /// `wasm:<export>`: looked up in the component's exports.
    Wasm,
    /// `ts:<path>`: exists on disk and transpiles cleanly.
    Script,
}

impl ControllerRef {
    /// Parses a reference; `None` when it carries no known prefix or an empty target.
    #[must_use]
    pub fn parse(reference: &str) -> Option<Self> {
        let (prefix, target) = reference.split_once(':')?;
        if target.is_empty() || target.contains(':') {
            return None;
        }
        let kind = match prefix {
            "native" => ControllerKind::Native,
            "wasm" => ControllerKind::Wasm,
            "ts" => ControllerKind::Script,
            _ => return None,
        };
        Some(Self {
            reference: reference.to_owned(),
            kind,
        })
    }

    /// The part after the prefix: the name, export or script path.
    #[must_use]
    pub fn target(&self) -> &str {
        self.reference.split_once(':').map_or("", |(_, r)| r)
    }
}

/// The outcome of [`check_app`]: the parsed manifest (when the manifest itself is sound) and
/// every problem found, ordered by code then path so the report is stable.
#[derive(Debug, Clone)]
pub struct CheckReport {
    /// The parsed manifest, when `manifest.toml` parsed and validated.
    pub manifest: Option<AppManifest>,
    /// Every problem, in a stable order.
    pub problems: Vec<PackProblem>,
}

impl CheckReport {
    /// Whether the app passes: no problems at all.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Checks an app directory end to end and reports **every** problem at once.
///
/// `native_names` is the optional list of controller names this build provides: `None`
/// skips the `native:` check, `Some(names)` refuses a `native:` name outside it.
pub fn check_app(dir: &Path, native_names: Option<&BTreeSet<String>>) -> CheckReport {
    let mut problems = Vec::new();
    let app = AppDir::read(dir, &mut problems);
    let limits = Limits::new();

    // 1. The manifest exists, parses and validates.
    let manifest = match app.manifest.clone() {
        None => {
            problems.push(PackProblem::new(
                crate::error::MANIFEST_MISSING,
                format!("the app has no {} under {}", MANIFEST_FILE, dir.display()),
            ));
            problems.sort();
            return CheckReport {
                manifest: None,
                problems,
            };
        }
        Some(Ok(manifest)) => Some(manifest),
        Some(Err(errs)) => {
            for err in errs {
                problems.push(PackProblem::about(
                    crate::error::MANIFEST,
                    MANIFEST_FILE,
                    err.to_string(),
                ));
            }
            None
        }
    };

    // 2. The layout: allowed places only, no escapes, within the limits.
    for fault in &app.faults {
        problems.push(match fault {
            LayoutFault::Outside(path) => PackProblem::about(
                crate::error::LAYOUT,
                path.clone(),
                "a bundle carries manifest.toml, app.wasm, controllers.toml, doctypes/, \
                 permissions/, assets/, fixtures/, patches/ and scripts/ only",
            ),
            LayoutFault::Escape(path) => PackProblem::about(
                crate::error::PATH_ESCAPE,
                path.clone(),
                "this name would escape the app directory (a `..` segment, an absolute path, \
                 a backslash, a colon, an empty or `.` segment); it is not packable",
            ),
        });
    }
    let mut total: u64 = 0;
    for file in &app.files {
        total = total.saturating_add(file.bytes.len() as u64);
        if file.bytes.len() as u64 > limits.max_file_bytes {
            problems.push(PackProblem::about(
                crate::error::OVERSIZED,
                file.rel.clone(),
                format!(
                    "the file is {} bytes; the limit for one file is {}",
                    file.bytes.len(),
                    limits.max_file_bytes
                ),
            ));
        }
    }
    if total > limits.max_total_bytes {
        problems.push(PackProblem::new(
            crate::error::OVERSIZED,
            format!(
                "the app's files total {total} bytes; the limit is {}",
                limits.max_total_bytes
            ),
        ));
    }
    if (app.files.len() as u64) > limits.max_entries as u64 {
        problems.push(PackProblem::new(
            crate::error::TOO_MANY_FILES,
            format!(
                "the app has {} files; the limit is {}",
                app.files.len(),
                limits.max_entries
            ),
        ));
    }

    // 3. DocTypes: every definition loads, then the merged set runs spot-meta's rules and
    // lints (`Registry::build`), so the author sees schema faults and not just parse faults.
    let definitions = match spot_meta::load_dir(&dir.join(layout::DOCTYPES_DIR)) {
        Ok(definitions) => definitions,
        Err(errors) => {
            for err in errors {
                let path = match &err.file {
                    Some(file) => format!("{}/{}", layout::DOCTYPES_DIR, file),
                    None => layout::DOCTYPES_DIR.to_owned(),
                };
                problems.push(PackProblem::about(
                    crate::error::DOCTYPE,
                    path,
                    err.to_string(),
                ));
            }
            Vec::new()
        }
    };
    if let Err(errors) = spot_meta::Registry::build(definitions, &spot_meta::Customisations::new())
    {
        for err in errors {
            // The rules' errors name the DocType, not the file; the layout's convention is
            // one file per DocType (`<name>.toml`), so the report puts the fault there.
            let path = match &err.file {
                Some(file) => format!("{}/{}", layout::DOCTYPES_DIR, file),
                None => match &err.doctype {
                    Some(doctype) => {
                        format!("{}/{}.toml", layout::DOCTYPES_DIR, doctype)
                    }
                    None => layout::DOCTYPES_DIR.to_owned(),
                },
            };
            problems.push(PackProblem::about(
                crate::error::DOCTYPE,
                path,
                err.to_string(),
            ));
        }
    }

    // 4. The permission model: one load over the four files, spot-permissions' own rules.
    let texts = ModelTexts {
        grants: read_text(dir, layout::GRANTS_FILE),
        field_grants: read_text(dir, layout::FIELD_GRANTS_FILE),
        groups: read_text(dir, layout::GROUPS_FILE),
        shares: read_text(dir, layout::SHARES_FILE),
    };
    if let Err(errors) = spot_permissions::load(&texts) {
        for err in errors.as_slice() {
            // The loader's place names the table ("grants", "field_grants", "groups",
            // "shares") or a row ("grants.entries[0]"); the report wants the app-relative
            // file the row came from, so the author can open it.
            let place = err.place();
            let file = place
                .split(['.', '['])
                .next()
                .unwrap_or(place)
                .replace("grants", layout::GRANTS_FILE)
                .replace("field_grants", layout::FIELD_GRANTS_FILE)
                .replace("groups", layout::GROUPS_FILE)
                .replace("shares", layout::SHARES_FILE);
            problems.push(PackProblem::about(
                crate::error::PERMISSIONS,
                file,
                err.to_string(),
            ));
        }
    }

    // 5. Controllers: every step something names must resolve; each reference must check.
    if let Some(manifest) = &manifest {
        for step in ControllersFile::required_steps(manifest) {
            if app.controllers.reference_of(&step).is_none() {
                problems.push(PackProblem::about(
                    crate::error::CONTROLLER_MISSING,
                    CONTROLLERS_FILE,
                    format!("the manifest names the step `{step}` but no controller provides it"),
                ));
            }
        }
    }
    let mut references: BTreeSet<&str> = app
        .controllers
        .steps
        .iter()
        .map(|(_, r)| r.as_str())
        .collect();
    if let Some(manifest) = &manifest {
        for step in ControllersFile::required_steps(manifest) {
            if let Some(reference) = app.controllers.reference_of(&step) {
                references.insert(reference);
            }
        }
    }
    for reference in &references {
        let Some(parsed) = ControllerRef::parse(reference) else {
            problems.push(PackProblem::about(
                crate::error::CONTROLLER_UNKNOWN,
                CONTROLLERS_FILE,
                format!(
                    "the reference `{reference}` is not `native:<name>`, `wasm:<export>` or \
                     `ts:<path>`"
                ),
            ));
            continue;
        };
        match parsed.kind {
            ControllerKind::Native => {
                if let Some(names) = native_names
                    && !names.contains(parsed.target())
                {
                    problems.push(PackProblem::about(
                        crate::error::CONTROLLER_UNKNOWN,
                        CONTROLLERS_FILE,
                        format!(
                            "the controller `{}` is not in this build's native controller list",
                            parsed.reference
                        ),
                    ));
                }
            }
            ControllerKind::Wasm => component_check(&app, parsed.target(), &mut problems),
            ControllerKind::Script => script_check(dir, parsed.target(), &mut problems),
        }
    }

    // 6. Assets named by the manifest must exist in the app directory.
    if let Some(manifest) = &manifest {
        for asset in &manifest.assets {
            if !dir.join(asset.path.as_str()).is_file() {
                problems.push(PackProblem::about(
                    crate::error::ASSET_UNPACKED,
                    asset.path.as_str().to_owned(),
                    "the manifest names this asset but the app does not carry the file",
                ));
            }
        }
    }

    problems.sort();
    CheckReport { manifest, problems }
}

// --- the component, its exports and the capability check ---------------------------------------

/// Checks `app.wasm` for the export a `wasm:` reference names, and runs the privilege check.
fn component_check(app: &AppDir, export: &str, problems: &mut Vec<PackProblem>) {
    let Some(bytes) = &app.wasm else {
        problems.push(PackProblem::about(
            crate::error::COMPONENT_MISSING,
            WASM_FILE,
            "the controllers name a wasm: export but the app has no app.wasm",
        ));
        return;
    };
    let component = match wasm_component::parse(bytes) {
        Ok(component) => component,
        Err(why) => {
            problems.push(PackProblem::about(crate::error::WASM, WASM_FILE, why));
            return;
        }
    };
    if !component.exports.iter().any(|name| name == export) {
        problems.push(PackProblem::about(
            crate::error::EXPORT_MISSING,
            WASM_FILE,
            format!("the component does not export `{export}`"),
        ));
    }
    privilege_check(app, &component, problems);
}

/// The manifest's capabilities against the component's imports, as the installer's check will
/// run them: a known import without its capability is refused here, and a declared capability
/// nothing exercises is reported.
fn privilege_check(
    app: &AppDir,
    component: &wasm_component::Component,
    problems: &mut Vec<PackProblem>,
) {
    let Some(Ok(manifest)) = &app.manifest else {
        return;
    };
    let manifest: &AppManifest = manifest;
    let names = &component.imports;
    for import in names {
        match crate::imports::need_of(import) {
            crate::imports::Need::Nothing => {}
            crate::imports::Need::Unknown => problems.push(PackProblem::about(
                crate::error::UNKNOWN_IMPORT,
                WASM_FILE,
                format!("the component imports `{import}`, which the host does not provide"),
            )),
            crate::imports::Need::ReadDocTypes => {
                if manifest.capabilities.doctypes.read.is_empty() {
                    problems.push(PackProblem::about(
                        crate::error::IMPORT_WITHOUT_CAPABILITY,
                        MANIFEST_FILE,
                        format!(
                            "the component imports `{import}` but the manifest declares no \
                             DocType read access (`capabilities.doctypes.read`)"
                        ),
                    ));
                }
            }
        }
    }
    let reads_documents = names
        .iter()
        .any(|i| crate::imports::need_of(i) == crate::imports::Need::ReadDocTypes);
    if !manifest.capabilities.doctypes.read.is_empty() && !reads_documents {
        problems.push(PackProblem::about(
            crate::error::CAPABILITY_NOT_IMPORTED,
            MANIFEST_FILE,
            "the manifest declares DocType read access but the component imports nothing that \
             reads documents (`get-doc`)",
        ));
    }
    if !manifest.capabilities.doctypes.write.is_empty() {
        problems.push(PackProblem::about(
            crate::error::CAPABILITY_NOT_IMPORTED,
            MANIFEST_FILE,
            format!(
                "the manifest declares write access to {} DocTypes but the component imports \
                 nothing that changes documents",
                manifest.capabilities.doctypes.write.len()
            ),
        ));
    }
    if !manifest.capabilities.network.is_empty() {
        problems.push(PackProblem::about(
            crate::error::CAPABILITY_NOT_IMPORTED,
            MANIFEST_FILE,
            format!(
                "the manifest declares {} network endpoints but the component imports nothing \
                 that calls them",
                manifest.capabilities.network.len()
            ),
        ));
    }
    if manifest.capabilities.email {
        problems.push(PackProblem::about(
            crate::error::CAPABILITY_NOT_IMPORTED,
            MANIFEST_FILE,
            "the manifest declares the e-mail capability but the component imports nothing \
             that sends mail",
        ));
    }
    if manifest.capabilities.files != spot_manifest::FileAccess::None {
        problems.push(PackProblem::about(
            crate::error::CAPABILITY_NOT_IMPORTED,
            MANIFEST_FILE,
            "the manifest declares file-store access but the component imports nothing that \
             touches files",
        ));
    }
}

// --- scripts -----------------------------------------------------------------------------------

/// One `ts:` target: the file must exist and transpile with no diagnostics.
fn script_check(dir: &Path, target: &str, problems: &mut Vec<PackProblem>) {
    if !crate::pathsafe::is_safe_rel_path(target) {
        problems.push(PackProblem::about(
            crate::error::SCRIPT,
            CONTROLLERS_FILE,
            format!("the script path `{target}` is not a plain app-relative path"),
        ));
        return;
    }
    let path = dir.join(target);
    if !path.is_file() {
        problems.push(PackProblem::about(
            crate::error::SCRIPT,
            target.to_owned(),
            "the script does not exist in the app directory",
        ));
        return;
    }
    let source = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            problems.push(PackProblem::about(
                crate::error::SCRIPT,
                target.to_owned(),
                format!("the script could not be read: {err}"),
            ));
            return;
        }
    };
    if let Err(diagnostics) =
        spot_ts_transpile::transpile(&source, target, &spot_ts_transpile::Options::default())
    {
        let mut rendered: Vec<String> = diagnostics
            .iter()
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        rendered.sort();
        problems.push(PackProblem::about(
            crate::error::SCRIPT,
            target.to_owned(),
            format!(
                "the script does not transpile cleanly: {}",
                rendered.join("; ")
            ),
        ));
    }
}

/// Reads one text file; a missing file is the empty text the loaders accept.
fn read_text(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_default()
}

/// The binary pieces needed to build a bundle from a checked app.
pub(crate) struct PackInputs {
    /// The raw `manifest.toml`, as the author wrote it.
    pub manifest: String,
    /// The component binary; an app with no component packs an empty one.
    pub wasm: Vec<u8>,
    /// Every asset file, at its declared bundle path.
    pub assets: Vec<(String, Vec<u8>)>,
}

impl PackInputs {
    /// Collects the pieces of an app that passed [`check_app`].
    ///
    /// # Errors
    /// The `pack.*` problem when the manifest or an asset vanished between check and read.
    pub fn collect(
        dir: &Path,
        manifest: &AppManifest,
        problems: &mut Vec<PackProblem>,
    ) -> Result<Self, PackProblem> {
        let manifest_bytes = std::fs::read_to_string(dir.join(MANIFEST_FILE)).map_err(|err| {
            PackProblem::about(
                crate::error::MANIFEST,
                MANIFEST_FILE,
                format!("manifest.toml could not be read for packing: {err}"),
            )
        })?;
        let wasm = std::fs::read(dir.join(WASM_FILE)).unwrap_or_default();
        let mut assets = Vec::new();
        for asset in &manifest.assets {
            let bytes = match std::fs::read(dir.join(asset.path.as_str())) {
                Ok(bytes) => bytes,
                Err(err) => {
                    problems.push(PackProblem::about(
                        crate::error::ASSET_UNPACKED,
                        asset.path.as_str().to_owned(),
                        format!("the asset file could not be read: {err}"),
                    ));
                    continue;
                }
            };
            assets.push((asset.path.as_str().to_owned(), bytes));
        }
        Ok(Self {
            manifest: manifest_bytes,
            wasm,
            assets,
        })
    }
}
