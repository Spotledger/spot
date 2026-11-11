//! Inspecting a packed bundle: what `spot-pack inspect` reports.
//!
//! Everything here reads the bundle the way the installer would (`Bundle::open`), so the
//! report shows only what a validated bundle actually carries.

use std::collections::BTreeMap;

use spot_bundle::{Bundle, BundleError, HostInfo, Limits};

/// One entry of the bundle, as the inspector lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRow {
    /// The entry name.
    pub name: String,
    /// The inflated size in bytes.
    pub size: u64,
}

/// The inspect report of one bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectReport {
    /// The app id and version the manifest names.
    pub app_id: String,
    /// The manifest's version.
    pub version: String,
    /// The canonical digest, in hex.
    pub digest: String,
    /// The entries, sorted by name.
    pub entries: Vec<EntryRow>,
    /// The declared capabilities, in the manifest's own shape: network endpoints, e-mail,
    /// file access, DocType read and write lists.
    pub capabilities: CapabilitiesView,
    /// The steps the manifest names and the controller references this bundle's controllers
    /// data names (the bundle carries no controllers file: the steps alone are known).
    pub steps: Vec<String>,
    /// The component's import names, when the bundle carries a component.
    pub imports: Vec<String>,
    /// The component's export names, when the bundle carries one.
    pub exports: Vec<String>,
    /// The publisher, when the caller supplied a signature and certificate to check.
    pub publisher: Option<String>,
}

/// The manifest's capabilities, as the inspector shows them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitiesView {
    /// `host:port` endpoints, sorted.
    pub network: Vec<String>,
    /// Whether e-mail is declared.
    pub email: bool,
    /// The file access, as the manifest's word (`none`, `read`, `read_write`).
    pub files: String,
    /// DocTypes the app may read, sorted.
    pub doctypes_read: Vec<String>,
    /// DocTypes the app may write, sorted.
    pub doctypes_write: Vec<String>,
}

/// The manifest's `manifest.toml` entry name, spelled like [`crate::layout::MANIFEST_FILE`]
/// through the crate path (a file name, not an error code).
fn spot_pack_name_app_wasm() -> String {
    crate::layout::WASM_FILE.to_owned()
}

/// Opens `bytes` and reports its content as [`InspectReport`].
///
/// # Errors
/// The bundle's own problems when the bytes do not open.
pub fn inspect_bytes(
    bytes: &[u8],
    limits: &Limits,
    host: &HostInfo,
    publisher: Option<String>,
) -> Result<InspectReport, Vec<BundleError>> {
    let bundle = Bundle::open(bytes, limits, host)?;
    Ok(from_bundle(&bundle, publisher))
}

/// The report for an already-open bundle.
#[must_use]
pub fn from_bundle(bundle: &Bundle, publisher: Option<String>) -> InspectReport {
    let manifest = bundle.manifest();
    let mut entries: BTreeMap<String, u64> = BTreeMap::new();
    entries.insert(spot_pack_name_app_wasm(), bundle.wasm().len() as u64);
    entries.insert(
        "manifest.toml".to_owned(),
        bundle.manifest_bytes().len() as u64,
    );
    for (name, bytes) in bundle.assets() {
        entries.insert(name.clone(), bytes.len() as u64);
    }
    let entry_rows = entries
        .into_iter()
        .map(|(name, size)| EntryRow { name, size })
        .collect();
    let caps = &manifest.capabilities;
    let mut network = caps
        .network
        .iter()
        .map(|e| format!("{}:{}", e.host, e.port))
        .collect::<Vec<_>>();
    network.sort();
    let mut read = caps.doctypes.read.clone();
    read.sort();
    let mut write = caps.doctypes.write.clone();
    write.sort();
    let mut steps: Vec<String> = Vec::new();
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
    steps.sort();
    steps.dedup();
    let (imports, exports) = match wasm_bundle_parts(bundle) {
        Some((imports, exports)) => (imports, exports),
        None => (Vec::new(), Vec::new()),
    };
    InspectReport {
        app_id: manifest.app.name.as_str().to_owned(),
        version: manifest.app.version.to_string(),
        digest: bundle.digest().to_hex(),
        entries: entry_rows,
        capabilities: CapabilitiesView {
            network,
            email: caps.email,
            files: files_word(caps.files),
            doctypes_read: read,
            doctypes_write: write,
        },
        steps,
        imports,
        exports,
        publisher,
    }
}

/// The component's import and export names, when the bundle carries a parseable one.
fn wasm_bundle_parts(bundle: &Bundle) -> Option<(Vec<String>, Vec<String>)> {
    let component = crate::wasm_component::parse(bundle.wasm()).ok()?;
    Some((component.imports, component.exports))
}

/// The manifest's word for a file access level.
fn files_word(files: spot_manifest::FileAccess) -> String {
    match files {
        spot_manifest::FileAccess::None => "none".to_owned(),
        spot_manifest::FileAccess::Read => "read".to_owned(),
        spot_manifest::FileAccess::ReadWrite => "read_write".to_owned(),
    }
}
