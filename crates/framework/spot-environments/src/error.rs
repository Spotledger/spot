//! The crate's error type: a stable `environments.*` code with a plain-language message.
//!
//! Every code here has one entry in `crates/framework/spot-errors/catalogue/environments.toml`
//! with a summary and a remedy. A message never carries a secret value, a raw query or a
//! signing key: it names the item, never the content.

use std::fmt;

/// The stable `environments.*` codes this crate raises. Kept as constants so the raise sites and
/// the catalogue can be compared mechanically.
pub mod codes {
    /// The set's name, apps, members or order rules do not hold.
    pub const INVALID_SET: &str = "environments.invalid_set";
    /// A member has an order or role that cannot appear in a promotion ladder.
    pub const INVALID_MEMBER: &str = "environments.invalid_member";
    /// A linked tenant's region or app set differs from the set's.
    pub const LINK_MISMATCH: &str = "environments.link_mismatch";
    /// A linked tenant is not active, so it cannot receive a bundle.
    pub const TENANT_NOT_ACTIVE: &str = "environments.tenant_not_active";
    /// A promotion skipped an environment the policy does not allow skipping.
    pub const SKIP_REFUSED: &str = "environments.skip_refused";
    /// The tenant or role is not a member of the set.
    pub const UNKNOWN_MEMBER: &str = "environments.unknown_member";
    /// A set with that name is already stored.
    pub const STORE_NAME_TAKEN: &str = "environments.store_name_taken";
    /// No set with that name is stored.
    pub const STORE_NOT_FOUND: &str = "environments.store_not_found";
    /// The stored set changed since it was read.
    pub const STORE_VERSION_CONFLICT: &str = "environments.store_version_conflict";
    /// The store could not be read or written.
    pub const STORE_FAILED: &str = "environments.store_failed";
    /// The package's bytes are not a readable customization bundle.
    pub const INVALID_BUNDLE: &str = "environments.invalid_bundle";
    /// The envelope's issuer is not a root this store trusts.
    pub const UNKNOWN_SIGNER: &str = "environments.unknown_signer";
    /// The envelope's signature does not verify over its identity and digest.
    pub const SIGNATURE_INVALID: &str = "environments.signature_invalid";
    /// The signing key is not the one the certificate names.
    pub const KEY_MISMATCH: &str = "environments.key_mismatch";
    /// The envelope's digest is not the bundle's digest.
    pub const DIGEST_MISMATCH: &str = "environments.digest_mismatch";
    /// The envelope's bundle version is not the bundle's version.
    pub const VERSION_MISMATCH: &str = "environments.version_mismatch";
    /// The package was not addressed to this environment.
    pub const WRONG_TARGET: &str = "environments.wrong_target";
    /// The exact bundle digest was revoked.
    pub const BUNDLE_REVOKED: &str = "environments.bundle_revoked";
    /// A placeholder has no binding in the environment's variables.
    pub const UNRESOLVED_VARIABLE: &str = "environments.unresolved_variable";
    /// A variable name or placeholder is malformed, or two names share one value.
    pub const INVALID_VARIABLE: &str = "environments.invalid_variable";
    /// The applied target does not capture back to the source digest.
    pub const RESULT_MISMATCH: &str = "environments.result_mismatch";
    /// A promotion record's sequence, link or hash does not hold.
    pub const CHAIN_BROKEN: &str = "environments.chain_broken";
    /// Planning the bundle against the target site was refused.
    pub const PLAN_FAILED: &str = "environments.plan_failed";
    /// Applying the accepted plan to the target site failed.
    pub const APPLY_FAILED: &str = "environments.apply_failed";
    /// Capturing the target site after the apply failed.
    pub const CAPTURE_FAILED: &str = "environments.capture_failed";
}

/// An `environments.*` error with the item it concerns and one plain-language sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentError {
    code: &'static str,
    item: String,
    message: String,
}

impl EnvironmentError {
    /// An error with a code and a message, naming no item.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            item: String::new(),
            message: message.into(),
        }
    }

    /// An error about one item (a set, an environment, a variable).
    #[must_use]
    pub fn about(code: &'static str, item: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            item: item.into(),
            message: message.into(),
        }
    }

    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The item the error is about, if any.
    #[must_use]
    pub fn item(&self) -> &str {
        &self.item
    }

    /// The plain-language message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for EnvironmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.item.is_empty() {
            write!(f, "{}: {}", self.code, self.message)
        } else {
            write!(f, "{} ({}): {}", self.code, self.item, self.message)
        }
    }
}

impl std::error::Error for EnvironmentError {}

/// The crate's result type.
pub type Result<T> = std::result::Result<T, EnvironmentError>;
