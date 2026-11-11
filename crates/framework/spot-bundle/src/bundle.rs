//! Reading and validating a bundle: everything `Bundle::open` can find, it reports — not
//! only the first problem.
//!
//! The zip reading itself is a small trait ([`Reader`]) over [`zip::ZipArchive`]. The tests
//! supply their own implementation from hand-built entries, so rejection paths (a bomb, a
//! bad deflate stream, duplicate names) do not need a real archive to be exercised.

use std::collections::BTreeMap;

use sha2::{Digest as _, Sha256};
use zip::ZipArchive;
use zip::result::ZipError;

use crate::error::{
    APP_WASM, BAD_ARCHIVE, BAD_DEFLATE, BAD_MANIFEST, BAD_SYMLINK, BundleError, ComponentFault,
    DUPLICATE_ENTRY, HOST_REQUIRED, ILLEGAL_PATH, MANIFEST_ENTRY, MISSING_ASSET, MISSING_ENTRY,
    TOO_LARGE, TOO_MANY_ENTRIES, UNDECLARED_ASSET, UNEXPECTED_ENTRY,
};
use crate::limits::Limits;
use crate::paths::{PathError, SafeName, case_key, normalise, under_assets};
use crate::policy::UnusedCapability;
use spot_manifest::AppManifest;

/// One inflated entry of the archive, in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEntry {
    /// The name after normalisation.
    pub(crate) name: SafeName,
    /// The inflated bytes.
    pub(crate) bytes: Vec<u8>,
}

/// The archive as this crate reads it: every plain file entry, inflated, in archive order,
/// with each read failure already turned into a `bundle.*` problem.
///
/// Validation runs on this; the [`Reader`] builds it entry by entry, so the total-size and
/// single-file limits are checked **while inflating** — a bomb dies as soon as the inflated
/// bytes exceed the budget, never after allocating its full size.
#[derive(Debug, Default)]
pub(crate) struct Archive {
    /// Every plain file entry, in archive order.
    pub(crate) entries: Vec<RawEntry>,
    /// Every directory entry's normalised name, in archive order.
    pub(crate) dirs: Vec<SafeName>,
    /// Every problem seen while reading; may be non-empty even when some entries read.
    pub(crate) faults: Vec<crate::error::BundleError>,
}

/// Why one entry could not be inflated.
#[derive(Debug)]
pub(crate) enum ReadFault {
    /// The entry inflates past its budget: a bomb, cut off early.
    OverBudget,
    /// The zip layer failed: bad deflate, a CRC mismatch, encryption, a lost entry.
    Zip(ZipError),
}

/// Where a bundle is read from: the low-level zip operations this crate needs, so the
/// rejection paths are testable without a zip writer.
pub(crate) trait Reader {
    /// How many entries the archive holds, counting directory entries.
    fn entry_count(&self) -> u32;
    /// The raw (not normalised) name of entry `i`, plus whether it is a directory, a
    /// symlink, or encrypted.
    fn entry_meta(&mut self, i: usize) -> Result<EntryMeta, ZipError>;
    /// Inflates entry `i` in full and verifies its CRC, but never produces more than
    /// `budget` bytes: past that it returns [`ReadFault::OverBudget`].
    fn read_entry(&mut self, i: usize, budget: u64) -> Result<Vec<u8>, ReadFault>;
}

/// What [`Reader::entry_meta`] reports about one entry.
#[derive(Debug, Clone)]
pub(crate) struct EntryMeta {
    /// The raw name bytes, as stored.
    pub(crate) raw_name: Vec<u8>,
    /// Whether the entry is a directory (name ends with `/`).
    pub(crate) is_dir: bool,
    /// Whether the entry is a symlink (unix mode says so).
    pub(crate) is_symlink: bool,
    /// Whether the entry is encrypted.
    pub(crate) encrypted: bool,
}

/// The real reader over a [`zip::ZipArchive`]; CRC mismatches and bad streams surface as
/// `ZipError::Io` here, which the mapping below renders as `bundle.bad_deflate`.
impl<R: std::io::Read + std::io::Seek> Reader for ZipArchive<R> {
    fn entry_count(&self) -> u32 {
        zip::ZipArchive::len(self) as u32
    }

    fn entry_meta(&mut self, i: usize) -> Result<EntryMeta, ZipError> {
        let file = self.by_index_raw(i)?;
        Ok(EntryMeta {
            raw_name: file.name_raw().to_vec(),
            is_dir: file.is_dir(),
            is_symlink: file.is_symlink(),
            encrypted: file.encrypted(),
        })
    }

    fn read_entry(&mut self, i: usize, budget: u64) -> Result<Vec<u8>, ReadFault> {
        let mut file = self.by_index(i).map_err(ReadFault::Zip)?;
        let mut out = Vec::new();
        let mut chunk = [0_u8; 65_536];
        loop {
            match std::io::Read::read(&mut file, &mut chunk) {
                Ok(0) => return Ok(out),
                Ok(n) => {
                    out.extend_from_slice(&chunk[..n]);
                    if out.len() as u64 > budget {
                        return Err(ReadFault::OverBudget);
                    }
                }
                Err(err) => return Err(ReadFault::Zip(ZipError::from(err))),
            }
        }
    }
}

/// Reads the whole archive within `limits`, collecting every problem it can find.
pub(crate) fn read_archive(reader: &mut dyn Reader, limits: &Limits) -> Archive {
    let count = reader.entry_count();
    let mut archive = Archive::default();
    if count > limits.max_entries {
        archive.faults.push(crate::error::BundleError::new(
            TOO_MANY_ENTRIES,
            format!(
                "the bundle has {count} entries; the limit is {}",
                limits.max_entries
            ),
        ));
    }
    let mut total: u64 = 0;
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    // Over the limit, entries are still listed (they may hide path problems), only the ones
    // within the limit enter the validation set; the count fault is already recorded.
    let n = count.min(limits.max_entries) as usize;
    for i in 0..n {
        let meta = match reader.entry_meta(i) {
            Ok(meta) => meta,
            Err(err) => {
                archive.faults.push(zip_fault(i, err));
                continue;
            }
        };
        let name = match normalise(&meta.raw_name) {
            Ok(name) => name,
            Err(fault) => {
                archive.faults.push(path_fault(&meta.raw_name, fault));
                continue;
            }
        };
        if meta.encrypted {
            archive.faults.push(crate::error::BundleError::about(
                BAD_DEFLATE,
                name.as_str().to_owned(),
                "the entry is encrypted; bundles carry plain entries only".to_owned(),
            ));
            continue;
        }
        if meta.is_symlink {
            archive.faults.push(crate::error::BundleError::about(
                BAD_SYMLINK,
                name.as_str().to_owned(),
                "a bundle carries plain files and directories, not symlinks".to_owned(),
            ));
            continue;
        }
        if meta.is_dir {
            archive.dirs.push(name);
            continue;
        }
        // Duplicates: exact after normalisation, or colliding on Windows (case-only).
        let key = case_key(&name);
        if let Some(first) = seen.get(&key) {
            let why = if archive.entries[*first].name == name {
                "the same name appears twice"
            } else {
                "a second name that differs only by case (they collide on Windows)"
            };
            archive.faults.push(crate::error::BundleError::about(
                DUPLICATE_ENTRY,
                name.as_str().to_owned(),
                why.to_owned(),
            ));
            continue;
        }
        let file_budget = limits.max_file_bytes.min(limits.max_total_bytes - total);
        let bytes = match reader.read_entry(i, file_budget) {
            Ok(bytes) => bytes,
            Err(ReadFault::OverBudget) => {
                archive.faults.push(crate::error::BundleError::about(
                    TOO_LARGE,
                    name.as_str().to_owned(),
                    format!(
                        "the file inflates past {file_budget} bytes (the limit for one file, \
                         or what is left of the total); refused before its full size was \
                         allocated"
                    ),
                ));
                continue;
            }
            Err(ReadFault::Zip(err)) => {
                archive.faults.push(zip_fault(i, err));
                continue;
            }
        };
        total += bytes.len() as u64;
        seen.insert(key, archive.entries.len());
        archive.entries.push(RawEntry { name, bytes });
    }
    archive
}

/// Turns a zip failure into a `bundle.*` problem, with the entry's index where no name is
/// known.
fn zip_fault(index: usize, err: ZipError) -> crate::error::BundleError {
    match err {
        ZipError::InvalidArchive(why) => crate::error::BundleError::new(
            BAD_ARCHIVE,
            format!("entry {index} of the archive is malformed: {why}"),
        ),
        ZipError::UnsupportedArchive(why) => crate::error::BundleError::new(
            BAD_ARCHIVE,
            format!("entry {index} uses a feature this bundle format does not support: {why}"),
        ),
        ZipError::FileNotFound => crate::error::BundleError::new(
            BAD_ARCHIVE,
            format!("the archive lost entry {index} between listing and reading"),
        ),
        other => crate::error::BundleError::new(
            BAD_DEFLATE,
            format!("entry {index} could not be read: {other}"),
        ),
    }
}

/// Turns a path error into its `bundle.illegal_path` problem, naming the raw entry.
pub(crate) fn path_fault(raw: &[u8], fault: PathError) -> crate::error::BundleError {
    let shown = String::from_utf8_lossy(raw).into_owned();
    match fault {
        PathError::Empty => crate::error::BundleError::about(
            ILLEGAL_PATH,
            shown,
            "the name is empty after normalisation (it held only `.` and `/`)".to_owned(),
        ),
        PathError::NotRelative(why) => crate::error::BundleError::about(
            ILLEGAL_PATH,
            shown.clone(),
            format!("the entry name {shown:?}: {why}"),
        ),
        PathError::NotUtf8 => crate::error::BundleError::about(
            ILLEGAL_PATH,
            shown,
            "the name is not valid UTF-8".to_owned(),
        ),
    }
}

/// The classified structure of a read archive: which entry is which.
#[derive(Debug, Default)]
pub(crate) struct Layout {
    /// `manifest.toml`, when present.
    pub(crate) manifest: Option<Vec<u8>>,
    /// `app.wasm`, when present.
    pub(crate) wasm: Option<Vec<u8>>,
    /// Every file under `assets/`, normalised name to content.
    pub(crate) assets: BTreeMap<String, Vec<u8>>,
    /// Everything that is none of the three.
    pub(crate) stray: Vec<SafeName>,
}

impl Layout {
    /// Sorts a read archive into manifest, component and assets, and flags every entry that
    /// belongs to none of them.
    pub(crate) fn from(archive: &Archive) -> Layout {
        let mut layout = Layout::default();
        for entry in &archive.entries {
            let name = entry.name.as_str();
            match name {
                MANIFEST_ENTRY => layout.manifest = Some(entry.bytes.clone()),
                APP_WASM => layout.wasm = Some(entry.bytes.clone()),
                _ if under_assets(&entry.name) => {
                    layout.assets.insert(name.to_owned(), entry.bytes.clone());
                }
                _ => layout.stray.push(entry.name.clone()),
            }
        }
        layout
    }
}

/// The checked content of a bundle.
#[derive(Debug, Clone)]
pub(crate) struct Core {
    /// The parsed manifest.
    pub(crate) manifest: AppManifest,
    /// The raw `manifest.toml` bytes, for the canonical digest.
    pub(crate) manifest_bytes: Vec<u8>,
    /// The component binary.
    pub(crate) wasm: Vec<u8>,
    /// The asset files, normalised name to inflated bytes.
    pub(crate) assets: BTreeMap<String, Vec<u8>>,
}

/// Classifies the read archive and validates manifest, component and asset set. Returns the
/// core bundle content, or **every** problem found — shape problems first, then the
/// manifest's, the component's, the asset set's and the host version's.
pub(crate) fn classify_and_check(
    archive: &Archive,
    host: &crate::HostInfo,
) -> Result<Core, Vec<crate::error::BundleError>> {
    let mut errs: Vec<crate::error::BundleError> = Vec::new();
    let layout = Layout::from(archive);
    // Shape: required entries present, nothing outside the fixed layout.
    if layout.manifest.is_none() {
        errs.push(crate::error::BundleError::new(
            MISSING_ENTRY,
            "the bundle has no manifest.toml",
        ));
    }
    if layout.wasm.is_none() {
        errs.push(crate::error::BundleError::new(
            MISSING_ENTRY,
            "the bundle has no app.wasm (it must be a WebAssembly component)",
        ));
    }
    for name in &layout.stray {
        errs.push(crate::error::BundleError::about(
            UNEXPECTED_ENTRY,
            name.as_str().to_owned(),
            "a bundle carries manifest.toml, app.wasm and assets/** only (directory entries \
             are allowed and ignored)"
                .to_owned(),
        ));
    }
    let Some(manifest_bytes) = layout.manifest else {
        return Err(errs);
    };
    let Some(wasm) = layout.wasm else {
        return Err(errs);
    };
    match std::str::from_utf8(&manifest_bytes) {
        Ok(source) => match spot_manifest::parse(source, MANIFEST_ENTRY) {
            Ok(manifest) => {
                // The component: right header, parses and validates as a component.
                if let Err(fault) = check_component(&wasm) {
                    errs.push(fault.into());
                    return Err(errs);
                }
                // The asset set, and the host version.
                errs.extend(check_assets(&manifest, &layout.assets));
                if let Some(req) = &manifest.app.requires_host
                    && !req.matches(&host.version)
                {
                    errs.push(crate::error::BundleError::new(
                        HOST_REQUIRED,
                        format!(
                            "the app needs a host in {req}; this host is {}",
                            host.version
                        ),
                    ));
                }
                let core = Core {
                    manifest,
                    manifest_bytes,
                    wasm,
                    assets: layout.assets,
                };
                if errs.is_empty() { Ok(core) } else { Err(errs) }
            }
            Err(manifest_errs) => {
                let rendered = manifest_errs
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                errs.push(crate::error::BundleError::about(
                    BAD_MANIFEST,
                    MANIFEST_ENTRY,
                    format!("spot-manifest reported {rendered}"),
                ));
                Err(errs)
            }
        },
        Err(_) => {
            errs.push(crate::error::BundleError::about(
                BAD_MANIFEST,
                MANIFEST_ENTRY,
                "the manifest is not valid UTF-8".to_owned(),
            ));
            Err(errs)
        }
    }
}

/// Whether the component binary is acceptable: right header, parses and validates as a
/// component.
pub(crate) fn check_component(wasm: &[u8]) -> Result<(), ComponentFault> {
    const HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6D, 0x0D, 0x00, 0x01, 0x00];
    if wasm.len() < HEADER.len() || wasm[..8] != HEADER {
        return Err(ComponentFault::NotAComponent);
    }
    let mut validator =
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::default());
    validator
        .validate_all(wasm)
        .map(|_| ())
        .map_err(|err| ComponentFault::Invalid(err.to_string()))
}

/// The asset checks: every declared path exists, every file under `assets/` is declared.
fn check_assets(
    manifest: &AppManifest,
    present: &BTreeMap<String, Vec<u8>>,
) -> Vec<crate::error::BundleError> {
    let mut errs = Vec::new();
    // The manifest's `FilePath` already forbids `..` and the rest; the bundle normalises
    // again here so it keeps its own guarantee (defence in depth, one code for both).
    let mut declared: Vec<SafeName> = Vec::new();
    for asset in &manifest.assets {
        match normalise(asset.path.as_str().as_bytes()) {
            Ok(name) => declared.push(name),
            Err(fault) => errs.push(path_fault(asset.path.as_str().as_bytes(), fault)),
        }
    }
    for name in &declared {
        let wanted = name.as_str().to_owned();
        if !present.contains_key(&wanted) {
            errs.push(crate::error::BundleError::about(
                MISSING_ASSET,
                wanted,
                "the manifest declares this asset but the bundle does not carry it".to_owned(),
            ));
        }
    }
    let declared_keys: std::collections::BTreeSet<String> =
        declared.iter().map(|n| n.as_str().to_owned()).collect();
    for present_key in present.keys() {
        if !declared_keys.contains(present_key) {
            errs.push(crate::error::BundleError::about(
                UNDECLARED_ASSET,
                present_key.clone(),
                "the bundle carries this file under assets/ but the manifest does not declare \
                 it"
                .to_owned(),
            ));
        }
    }
    errs
}

/// The canonical listing one digest covers, in byte-wise sorted name order:
/// `prefix || len(name) | name | len(content) | content` per entry.
///
/// The prefix `spotledger-bundle-v1\0` keeps the digest a domain-specific value; names and
/// lengths are fixed-width little-endian, so concatenations can never coalesce ambiguously.
pub(crate) fn canonical_input(parts: &[(String, Vec<u8>)]) -> Vec<u8> {
    const PREFIX: &[u8] = b"spotledger-bundle-v1\0";
    let mut out = Vec::new();
    out.extend_from_slice(PREFIX);
    // The byte-wise sorted name order makes the digest independent of ZIP ordering,
    // timestamps, compression level and extra fields.
    let mut sorted: Vec<&(String, Vec<u8>)> = parts.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    for (name, content) in sorted {
        out.extend_from_slice(&(name.len() as u64).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(content.len() as u64).to_le_bytes());
        out.extend_from_slice(content);
    }
    out
}

/// SHA-256 over the canonical listing.
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// The digest input over a full bundle: manifest, component and assets (each entry under
/// its normalised name as stored in the bundle, `assets/` prefixes included).
pub(crate) fn digest_parts(
    manifest: &[u8],
    wasm: &[u8],
    assets: &BTreeMap<String, Vec<u8>>,
) -> Vec<(String, Vec<u8>)> {
    let mut parts = vec![
        (MANIFEST_ENTRY.to_owned(), manifest.to_vec()),
        (APP_WASM.to_owned(), wasm.to_vec()),
    ];
    parts.extend(
        assets
            .iter()
            .map(|(name, bytes)| (name.clone(), bytes.clone())),
    );
    parts
}

/// The host a bundle is opened for: the `requires_host` range in the manifest is checked
/// against this version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostInfo {
    /// This host's platform version.
    pub version: semver::Version,
}
/// The outcome of [`Bundle::check_privileges`]: the `bundle.*` errors that fail the bundle,
/// plus the declared capabilities nothing in the component exercises (warning level, for
/// the admin to read — never an error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Every problem that fails the bundle: unknown imports, imports without a declared
    /// capability, declarations beyond the policy.
    pub errors: Vec<BundleError>,
    /// Declared capabilities the component never exercises, as notes for the admin.
    pub unused_capabilities: Vec<UnusedCapability>,
}

impl Report {
    /// Whether the bundle passes the privilege check: no errors, whatever the warnings.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// The errors, for the plain `Vec<BundleError>` view of the check.
    pub fn into_errors(self) -> Vec<BundleError> {
        self.errors
    }
}

/// A validated bundle, held in memory.
///
/// Built only by [`Bundle::open`]; every field is read through methods, so the invariants
/// the checks established (path safety, limits, a component binary, a valid manifest, the
/// asset set) cannot be broken from outside.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// The checked content: manifest, component, assets.
    core: Core,
    /// The component's import names, from the privilege walk.
    imports: Vec<String>,
}

impl Bundle {
    /// Opens a bundle from bytes, applying `limits` and validating it against `host` —
    /// **without running anything**.
    ///
    /// Returns every problem it can find, not only the first: reading problems (path
    /// safety, duplicates, limits) and the manifest's own `manifest.*` errors come out
    /// together. An `Ok` bundle has passed all of them; the privilege check
    /// ([`Bundle::check_privileges`]) runs separately, because the policy is the host's
    /// decision, not the bundle's.
    pub fn open(
        bytes: &[u8],
        limits: &Limits,
        host: &HostInfo,
    ) -> Result<Bundle, Vec<BundleError>> {
        // The compressed bound first: no table is parsed before the input size is legal.
        if bytes.len() as u64 > limits.max_compressed_bytes {
            return Err(vec![BundleError::new(
                crate::error::TOO_LARGE,
                format!(
                    "the bundle is {} bytes; the limit is {}",
                    bytes.len(),
                    limits.max_compressed_bytes
                ),
            )]);
        }
        let mut zip = match ZipArchive::new(std::io::Cursor::new(bytes)) {
            Ok(zip) => zip,
            Err(err) => return Err(vec![open_fault(err)]),
        };
        let mut archive = read_archive(&mut zip, limits);
        let mut errs = std::mem::take(&mut archive.faults);
        // Duplicates need the central directory: the zip crate silently drops a repeat.
        if let Some(names) = central_directory_names(bytes) {
            errs.extend(duplicate_faults(&names));
        }
        match classify_and_check(&archive, host) {
            Ok(core) if errs.is_empty() => {
                let imports = crate::imports_check::component_import_names(&core.wasm)
                    .map_err(|fault| vec![BundleError::from(fault)])?;
                Ok(Bundle { core, imports })
            }
            Ok(_) => {
                // The content checks passed but the read found problems (an over-limit
                // directory entry, say): the read problems decide.
                Err(errs)
            }
            Err(mut more) => {
                errs.append(&mut more);
                Err(errs)
            }
        }
    }

    /// The parsed, validated manifest.
    pub fn manifest(&self) -> &AppManifest {
        &self.core.manifest
    }

    /// The component binary, as carried.
    pub fn wasm(&self) -> &[u8] {
        &self.core.wasm
    }

    /// The raw `manifest.toml`, as carried.
    pub fn manifest_bytes(&self) -> &[u8] {
        &self.core.manifest_bytes
    }

    /// The assets, at their normalised bundle-relative names.
    pub fn assets(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.core.assets
    }

    /// The canonical digest over the bundle's content: independent of ZIP ordering,
    /// timestamps, compression level and extra fields.
    pub fn digest(&self) -> crate::digest::Digest {
        let parts = digest_parts(
            &self.core.manifest_bytes,
            &self.core.wasm,
            &self.core.assets,
        );
        crate::digest::Digest::from_bytes(sha256(&canonical_input(&parts)))
    }

    /// The component's import names, in declaration order.
    pub fn component_imports(&self) -> &[String] {
        &self.imports
    }

    /// The privilege check: the component's imports against the capability table, and the
    /// manifest's declared capabilities against `policy`.
    ///
    /// Rejects a bundle that imports something the host does not know
    /// (`bundle.unknown_import`) or imports a capability-bearing function the manifest does
    /// not declare (`bundle.import_without_capability`), and a manifest that declares
    /// beyond `policy` (`bundle.over_privileged`, naming the capability and the limit).
    /// Declared capabilities the component never exercises come back as warnings, not
    /// errors.
    pub fn check_privileges(&self, policy: &crate::policy::Policy) -> Report {
        let (mut import_errs, unused) =
            crate::policy::check_imports(&self.imports, &self.manifest().capabilities);
        let mut declared_errs =
            crate::policy::over_privilege(&self.manifest().capabilities, policy);
        import_errs.append(&mut declared_errs);
        Report {
            errors: import_errs,
            unused_capabilities: unused,
        }
    }
}

/// Why the archive could not be opened at all.
fn open_fault(err: ZipError) -> BundleError {
    match err {
        ZipError::InvalidArchive(why) => {
            BundleError::new(BAD_ARCHIVE, format!("the archive is malformed: {why}"))
        }
        ZipError::UnsupportedArchive(why) => BundleError::new(
            BAD_ARCHIVE,
            format!("the archive uses a feature this bundle format does not support: {why}"),
        ),
        other => BundleError::new(
            BAD_ARCHIVE,
            format!("the archive could not be read: {other}"),
        ),
    }
}

/// Walks the archive's central directory and returns every entry's raw name, in directory
/// order — duplicates included.
///
/// `zip::ZipArchive` keys entries by name and silently drops a repeat, so the duplicate
/// check cannot run on what it exposes; the central directory is the format's authoritative
/// listing and lists every record. Classic EOCD only: a bundle is capped far below the
/// 4 GiB / 65 535-entry boundaries where zip64 would be needed.
pub(crate) fn central_directory_names(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    const EOCD: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
    const CD_HEADER: [u8; 4] = [0x50, 0x4B, 0x01, 0x02];
    // Find the EOCD from the back; it may carry a comment of up to 65 535 bytes.
    if bytes.len() < 22 {
        return None;
    }
    let mut eocd = None;
    for i in (0..=bytes.len() - 22).rev() {
        if bytes[i..i + 4] == EOCD {
            eocd = Some(i);
            break;
        }
    }
    let eocd = eocd?;
    let read16 = |off: usize| u16::from_le_bytes([bytes[off], bytes[off + 1]]);
    let read32 = |off: usize| {
        u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
    };
    let count = read16(eocd + 10) as usize;
    let cd_offset = read32(eocd + 16) as usize;
    let mut names = Vec::with_capacity(count);
    let mut cursor = cd_offset;
    for _ in 0..count {
        if cursor + 46 > bytes.len() || bytes[cursor..cursor + 4] != CD_HEADER {
            return None;
        }
        let name_len = read16(cursor + 28) as usize;
        let extra_len = read16(cursor + 30) as usize;
        let comment_len = read16(cursor + 32) as usize;
        let name_at = cursor + 46;
        if name_at + name_len > bytes.len() {
            return None;
        }
        names.push(bytes[name_at..name_at + name_len].to_vec());
        cursor = name_at + name_len + extra_len + comment_len;
    }
    Some(names)
}

/// Detects duplicate entry names from the central directory listing: exact repeats and
/// names differing only by ASCII case (they collide on Windows).
pub(crate) fn duplicate_faults(names: &[Vec<u8>]) -> Vec<crate::error::BundleError> {
    let mut errs = Vec::new();
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for raw in names {
        let Ok(name) = normalise(raw) else {
            // Path problems are reported by the reader; this pass only hunts duplicates.
            continue;
        };
        let key = case_key(&name);
        if let Some(first) = seen.get(&key) {
            let why = if first.as_str() == name.as_str() {
                "the same name appears twice in the bundle"
            } else {
                "a second name that differs only by case (they collide on Windows)"
            };
            errs.push(crate::error::BundleError::about(
                DUPLICATE_ENTRY,
                name.as_str().to_owned(),
                why.to_owned(),
            ));
            continue;
        }
        seen.insert(key, name.as_str().to_owned());
    }
    errs
}
