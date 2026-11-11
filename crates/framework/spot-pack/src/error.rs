//! The `pack.*` error family: every problem `check_app`, `pack_app`, `sign_bundle` and
//! `verify_bundle` can report, collected — not only the first.
//!
//! Every code this crate raises is catalogued in
//! `crates/framework/spot-errors/catalogue/pack.toml` (the error-code scan checks both
//! directions). Callers match on [`PackProblem::code`], never on the message.

use std::fmt;

/// `pack.io`: a file or directory could not be read.
pub const IO: &str = "pack.io";
/// `pack.layout`: a file sits outside the allowed app layout.
pub const LAYOUT: &str = "pack.layout";
/// `pack.path_escape`: a path that would leave the app directory.
pub const PATH_ESCAPE: &str = "pack.path_escape";
/// `pack.oversized`: a file exceeds the bundle limits.
pub const OVERSIZED: &str = "pack.oversized";
/// `pack.too_many_files`: more files than the bundle limits allow.
pub const TOO_MANY_FILES: &str = "pack.too_many_files";
/// `pack.controllers`: the controllers file is missing a needed entry or does not parse.
pub const CONTROLLERS: &str = "pack.controllers";
/// `pack.manifest`: `manifest.toml` failed to parse or validate.
pub const MANIFEST: &str = "pack.manifest";
/// `pack.manifest_missing`: the app has no `manifest.toml`.
pub const MANIFEST_MISSING: &str = "pack.manifest_missing";
/// `pack.doctype`: a DocType definition failed to load.
pub const DOCTYPE: &str = "pack.doctype";
/// `pack.permissions`: a permission model file failed to load.
pub const PERMISSIONS: &str = "pack.permissions";
/// `pack.controller_missing`: a step the manifest names has no controller reference.
pub const CONTROLLER_MISSING: &str = "pack.controller_missing";
/// `pack.controller_unknown`: a `native:` reference is not in the controller name list.
pub const CONTROLLER_UNKNOWN: &str = "pack.controller_unknown";
/// `pack.component_missing`: a `wasm:` reference names the component or the component is absent.
pub const COMPONENT_MISSING: &str = "pack.component_missing";
/// `pack.export_missing`: a `wasm:` reference names an export the component does not have.
pub const EXPORT_MISSING: &str = "pack.export_missing";
/// `pack.script`: a `ts:` file is missing or does not transpile cleanly.
pub const SCRIPT: &str = "pack.script";
/// `pack.asset_unpacked`: a manifest asset is not in the app directory.
pub const ASSET_UNPACKED: &str = "pack.asset_unpacked";
/// `pack.capability_not_imported`: a declared capability nothing in the code exercises.
pub const CAPABILITY_NOT_IMPORTED: &str = "pack.capability_not_imported";
/// `pack.import_without_capability`: the component imports a capability the manifest does not
/// declare — the installer's check, surfaced at pack time.
pub const IMPORT_WITHOUT_CAPABILITY: &str = "pack.import_without_capability";
/// `pack.unknown_import`: the component imports something the host does not provide.
pub const UNKNOWN_IMPORT: &str = "pack.unknown_import";
/// `pack.wasm`: the component does not parse as a WebAssembly component.
pub const WASM: &str = "pack.wasm";
/// `pack.sign`: signing failed (the key is not the certificate's).
pub const SIGN: &str = "pack.sign";
/// `pack.verify`: the signature check refused the bundle (the signing code is named inside).
pub const VERIFY: &str = "pack.verify";

/// One problem with an app or its bundle: a stable `pack.*` code, the app-relative file it
/// concerns, and a message a developer can act on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackProblem {
    /// The stable machine-readable code.
    pub code: &'static str,
    /// The app-relative path the problem concerns, when it is about one.
    pub path: Option<String>,
    /// What went wrong, phrased for the app author.
    pub message: String,
}

impl PackProblem {
    /// A problem that is not about one file.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            path: None,
            message: message.into(),
        }
    }

    /// A problem about one app-relative file.
    #[must_use]
    pub fn about(code: &'static str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            path: Some(path.into()),
            message: message.into(),
        }
    }
}

impl fmt::Display for PackProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}: {}", self.code, path, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for PackProblem {}

/// Why `sign_bundle` could not make a signature.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PackError {
    /// The signing key is not the certificate's key.
    #[error("the signing key is not the certificate's key")]
    KeyMismatch,
    /// The signing key file does not hold a usable Ed25519 key.
    #[error("the key file is not a usable signing key")]
    KeyUnreadable,
}

impl PackError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyMismatch | Self::KeyUnreadable => SIGN,
        }
    }
}

/// Why `verify_bundle` refused a bundle: the signing layer's refusal, verbatim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the signature check refused the bundle: {0}")]
pub struct VerifyRefusal(pub spot_bundle_signing::SigningError);

impl VerifyRefusal {
    /// The refused bundle's canonical digest, when the caller wants to display it.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.0.code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_code_then_path_then_message() {
        let p = PackProblem::about(DOCTYPE, "doctypes/customer.toml", "unknown key");
        assert_eq!(
            p.to_string(),
            "pack.doctype: doctypes/customer.toml: unknown key"
        );
        let q = PackProblem::new(MANIFEST_MISSING, "no manifest");
        assert_eq!(q.to_string(), "pack.manifest_missing: no manifest");
    }
}
