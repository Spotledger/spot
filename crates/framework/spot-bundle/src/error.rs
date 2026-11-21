//! The `bundle.*` error family: every problem `Bundle::open` and the privilege check find,
//! collected — not only the first.

use std::fmt;

/// `bundle.bad_archive`: the bytes are not a ZIP archive at all.
pub(crate) const BAD_ARCHIVE: &str = "bundle.bad_archive";
/// `bundle.unexpected_entry`: an entry that is not `manifest.toml`, `app.wasm` or under
/// `assets/`.
pub(crate) const UNEXPECTED_ENTRY: &str = "bundle.unexpected_entry";
/// `bundle.missing_entry`: `manifest.toml` or `app.wasm` is absent.
pub(crate) const MISSING_ENTRY: &str = "bundle.missing_entry";
/// `bundle.illegal_path`: a name that escapes the bundle or is not a plain relative path.
pub(crate) const ILLEGAL_PATH: &str = "bundle.illegal_path";
/// `bundle.duplicate_entry`: two entries normalise to the same name, including two that
/// differ only by case (they collide on Windows).
pub(crate) const DUPLICATE_ENTRY: &str = "bundle.duplicate_entry";
/// `bundle.bad_symlink`: a symlink entry; bundles carry plain files and directories only.
pub(crate) const BAD_SYMLINK: &str = "bundle.bad_symlink";
/// `bundle.too_many_entries`: more entries than the limits allow.
pub(crate) const TOO_MANY_ENTRIES: &str = "bundle.too_many_entries";
/// `bundle.too_large`: the archive, one file, or the total inflated size exceeds the limits.
pub(crate) const TOO_LARGE: &str = "bundle.too_large";
/// `bundle.bad_deflate`: an entry does not decompress, or its CRC does not match.
pub(crate) const BAD_DEFLATE: &str = "bundle.bad_deflate";
/// `bundle.bad_manifest`: `manifest.toml` failed in `spot-manifest`.
pub(crate) const BAD_MANIFEST: &str = "bundle.bad_manifest";
/// `bundle.bad_component`: `app.wasm` is not a WebAssembly component, or does not parse as one.
pub(crate) const BAD_COMPONENT: &str = "bundle.bad_component";
/// `bundle.missing_asset`: an `[[assets]]` path the manifest declares is not in the bundle.
pub(crate) const MISSING_ASSET: &str = "bundle.missing_asset";
/// `bundle.undeclared_asset`: a file under `assets/` the manifest does not declare.
pub(crate) const UNDECLARED_ASSET: &str = "bundle.undeclared_asset";
/// `bundle.host_required`: `requires_host` excludes the host the bundle was opened for.
pub(crate) const HOST_REQUIRED: &str = "bundle.host_required";
/// `bundle.unknown_import`: the component imports something the host does not know.
pub(crate) const UNKNOWN_IMPORT: &str = "bundle.unknown_import";
/// `bundle.import_without_capability`: a known import whose capability the manifest does not
/// declare.
pub(crate) const IMPORT_WITHOUT_CAPABILITY: &str = "bundle.import_without_capability";
/// `bundle.over_privileged`: the manifest declares beyond the policy.
pub(crate) const OVER_PRIVILEGED: &str = "bundle.over_privileged";

/// One problem with a bundle: a stable `bundle.*` code, the entry it concerns, and a message
/// a developer can act on.
///
/// Callers match on [`code`](BundleError::code), never on the message: the codes are this
/// crate's API contract (see the crate docs for the full registry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleError {
    /// The stable machine-readable code, for the API error envelope.
    pub code: &'static str,
    /// The entry the error concerns, when it is about one.
    pub path: Option<String>,
    /// What went wrong, phrased for the app author or the admin.
    pub message: String,
}

impl BundleError {
    /// An error that is not about one entry.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            path: None,
            message: message.into(),
        }
    }

    /// An error about one entry of the archive.
    #[must_use]
    pub fn about(code: &'static str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            path: Some(path.into()),
            message: message.into(),
        }
    }
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}: {}", self.code, path, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for BundleError {}

/// Why `app.wasm` is not a component this host accepts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ComponentFault {
    /// The header is not `00 61 73 6d 0d 00 01 00`: not a component (maybe a core module,
    /// version 1), or not WebAssembly at all, or truncated.
    #[error(
        "app.wasm is not a WebAssembly component (the binary must start with the component header 00 61 73 6d 0d 00 01 00)"
    )]
    NotAComponent,
    /// `wasmparser` rejected the component.
    #[error("app.wasm does not parse as a component: {0}")]
    Invalid(String),
}

impl From<ComponentFault> for BundleError {
    fn from(fault: ComponentFault) -> Self {
        match fault {
            ComponentFault::NotAComponent => BundleError::about(
                BAD_COMPONENT,
                APP_WASM,
                "the binary must start with the component header 00 61 73 6d 0d 00 01 00 (a \
                 core module starts with 00 61 73 6d 01 00 00 00)"
                    .to_owned(),
            ),
            ComponentFault::Invalid(why) => BundleError::about(
                BAD_COMPONENT,
                APP_WASM,
                format!("it does not parse as a component: {why}"),
            ),
        }
    }
}

/// The two required entries, as the brief fixes them.
pub(crate) const MANIFEST_ENTRY: &str = "manifest.toml";
pub(crate) const APP_WASM: &str = "app.wasm";
