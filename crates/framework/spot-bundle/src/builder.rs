//! Packing a bundle deterministically: the same inputs always produce the same bytes.

use std::collections::BTreeMap;
use std::io::Write as _;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::{APP_WASM, BAD_DEFLATE, BundleError, MANIFEST_ENTRY, UNEXPECTED_ENTRY};

/// Builds a bundle from a manifest, the component and the assets.
///
/// The output is **deterministic**: entries are written in byte-wise sorted name order,
/// every entry carries the fixed 1980 timestamp (`DateTime::DEFAULT`, the zip crate's write
/// default), compression is DEFLATE at the default level, and no extra fields, permissions
/// or platform metadata are set. Two builders fed the same inputs produce byte-identical
/// archives, and the archive they produce has the same canonical [`crate::Digest`] as every
/// other packing of the same content.
///
/// The `spot` CLI packaging (WP-EXT-07) drives this; the tests drive it directly. Building
/// parses the manifest only as [`crate::Bundle::open`] will; packing a bundle that
/// validation will reject is deliberate, so the tests can build such bundles.
#[derive(Clone, Debug)]
pub struct BundleBuilder {
    /// The manifest TOML, as the author wrote it.
    manifest: String,
    /// The component binary.
    wasm: Vec<u8>,
    /// The assets, at their normalised bundle-relative paths.
    assets: BTreeMap<String, Vec<u8>>,
}

impl BundleBuilder {
    /// Starts a builder from the manifest TOML and the component binary.
    pub fn new(manifest: impl Into<String>, wasm: impl Into<Vec<u8>>) -> Self {
        Self {
            manifest: manifest.into(),
            wasm: wasm.into(),
            assets: BTreeMap::new(),
        }
    }

    /// Adds one asset file at `name` (bundle-relative, under `assets/`).
    ///
    /// The name must survive path normalisation and start with `assets/`, which
    /// [`BundleBuilder::build`] enforces: assets live in the assets folder, and a builder
    /// that accepted `../x` would undermine the validator.
    pub fn asset(mut self, name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        self.assets.insert(name.into(), bytes.into());
        self
    }

    /// Writes the deterministic ZIP.
    ///
    /// Every asset name is checked here — it must normalise to a safe relative path under
    /// `assets/` — and the archive is refused otherwise, with every bad name reported.
    pub fn build(&self) -> Result<Vec<u8>, Vec<BundleError>> {
        let mut errs = Vec::new();
        let mut safe_assets: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for (raw, bytes) in &self.assets {
            match crate::paths::normalise(raw.as_bytes()) {
                Ok(safe) if crate::paths::under_assets(&safe) => {
                    safe_assets.insert(safe.as_str().to_owned(), bytes.clone());
                }
                Ok(_) => errs.push(BundleError::about(
                    UNEXPECTED_ENTRY,
                    raw.clone(),
                    "assets live under `assets/` in the bundle; a builder will not place \
                     them elsewhere"
                        .to_owned(),
                )),
                Err(fault) => errs.push(crate::bundle::path_fault(raw.as_bytes(), fault)),
            }
        }
        if !errs.is_empty() {
            return Err(errs);
        }
        let mut pieces = vec![
            (
                MANIFEST_ENTRY.to_owned(),
                self.manifest.clone().into_bytes(),
            ),
            (APP_WASM.to_owned(), self.wasm.clone()),
        ];
        pieces.extend(
            safe_assets
                .iter()
                .map(|(name, bytes)| (name.clone(), bytes.clone())),
        );
        write_zip(&pieces).map_err(|err| vec![err])
    }
}

/// Writes `pieces` as a deterministic ZIP: sorted entries, fixed timestamp, DEFLATE.
///
/// Shared with the tests (which also build *non*-deterministic archives for the digest
/// tests through [`plain_zip_stored`]).
pub(crate) fn write_zip(pieces: &[(String, Vec<u8>)]) -> Result<Vec<u8>, BundleError> {
    let mut sorted: Vec<&(String, Vec<u8>)> = pieces.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let buffer = std::io::Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(buffer);
    for (name, bytes) in sorted {
        zip.start_file(
            name.clone(),
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .map_err(|err| {
            BundleError::new(
                BAD_DEFLATE,
                format!("the bundle could not be packed: {err}"),
            )
        })?;
        zip.write_all(bytes).map_err(|err| {
            BundleError::new(
                BAD_DEFLATE,
                format!("the bundle could not be packed: {err}"),
            )
        })?;
    }
    let buffer = zip.finish().map_err(|err| {
        BundleError::new(
            BAD_DEFLATE,
            format!("the bundle could not be closed: {err}"),
        )
    })?;
    Ok(buffer.into_inner())
}
