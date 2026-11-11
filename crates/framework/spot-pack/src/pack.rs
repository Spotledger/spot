//! Packing a checked app into one deterministic bundle.
//!
//! [`pack_app`] runs [`check_app`](crate::check_app) first, then builds the bundle through
//! `spot-bundle`'s [`BundleBuilder`] and re-opens it as the installer will. The bytes are
//! deterministic by construction (the builder sorts entries, pins the timestamp and sets no
//! host metadata), so the same directory packs to the same digest on every platform.

use std::path::Path;

use spot_bundle::{Bundle, BundleBuilder, BundleError, Digest, HostInfo, Limits};

use crate::check::{CheckReport, check_app};
use crate::error::PackProblem;

/// Options of a pack run.
#[derive(Clone, Debug)]
pub struct PackOptions {
    /// The host the packed bundle must open for; the default is platform 0.1.0, the
    /// workspace version.
    pub host: HostInfo,
    /// The bundle size limits; the default is the bundle crate's.
    pub limits: Limits,
    /// The native controller names of this build (`native:` references are checked against
    /// it when set).
    pub native_names: Option<std::collections::BTreeSet<String>>,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            host: HostInfo {
                version: semver::Version::new(0, 1, 0),
            },
            limits: Limits::new(),
            native_names: None,
        }
    }
}

/// What a successful pack produces.
#[derive(Debug, Clone)]
pub struct PackedBundle {
    /// The deterministic bundle bytes: manifest, component, assets, in the builder's order.
    pub bytes: Vec<u8>,
    /// The canonical digest of the content.
    pub digest: Digest,
}

impl PackedBundle {
    /// Opens the packed bytes as an installer would (`Bundle::open`), so the caller holds a
    /// validated [`Bundle`] without reading the file again.
    ///
    /// # Errors
    /// The bundle's own problems when the re-open fails (a bug here, not an author's).
    pub fn open(&self, options: &PackOptions) -> Result<Bundle, Vec<BundleError>> {
        Bundle::open(&self.bytes, &options.limits, &options.host)
    }
}

/// Packs an app directory into a deterministic bundle.
///
/// The check runs first; a report with problems is returned verbatim and nothing is built.
/// The manifest is packed **as the author wrote it**, the component under `app.wasm` and the
/// `[[assets]]` files at their declared bundle paths.
///
/// The error form boxes the check report (`result_large_err`); the facts are unchanged.
pub fn pack_app(
    dir: &Path,
    options: &PackOptions,
) -> Result<(CheckReport, PackedBundle), Box<CheckReport>> {
    let report = check_app(dir, options.native_names.as_ref());
    if !report.is_ok() {
        return Err(Box::new(report));
    }
    let Some(manifest) = report.manifest.clone() else {
        return Err(Box::new(report));
    };
    let mut problems = Vec::new();
    let inputs = match crate::check::PackInputs::collect(dir, &manifest, &mut problems) {
        Ok(inputs) => inputs,
        Err(problem) => {
            problems.push(problem);
            problems.sort();
            return Err(Box::new(CheckReport {
                manifest: Some(manifest),
                problems,
            }));
        }
    };
    if !problems.is_empty() {
        problems.extend(report.problems);
        problems.sort();
        return Err(Box::new(CheckReport {
            manifest: Some(manifest),
            problems,
        }));
    }

    let mut builder = BundleBuilder::new(inputs.manifest, inputs.wasm);
    for (name, bytes) in &inputs.assets {
        builder = builder.asset(name.clone(), bytes.clone());
    }
    let bytes = builder.build().map_err(|errs: Vec<BundleError>| {
        // The builder refuses only names `check_app` already vetted; surface its wording.
        Box::new(CheckReport {
            manifest: Some(manifest.clone()),
            problems: errs
                .iter()
                .map(|e| PackProblem::new(crate::error::WASM, e.to_string()))
                .collect(),
        })
    })?;
    let digest_bundle = Bundle::open(&bytes, &options.limits, &options.host).map_err(|errs| {
        Box::new(CheckReport {
            manifest: Some(manifest.clone()),
            problems: errs
                .iter()
                .map(|e| PackProblem::new(crate::error::WASM, e.to_string()))
                .collect(),
        })
    })?;
    let packed = PackedBundle {
        digest: digest_bundle.digest(),
        bytes,
    };
    Ok((report, packed))
}
