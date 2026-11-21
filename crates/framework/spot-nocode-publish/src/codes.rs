//! The stable `nocode_publish.*` codes this crate raises.
//!
//! Every code has one entry in `crates/framework/spot-errors/catalogue/nocode_publish.toml`
//! with the plain-language summary and remedy. The messages here name the item and what is
//! wrong, never a secret, a signature or a raw statement.

/// The draft as a whole is not publishable (a name, a version, a licence or the site id).
pub const BAD_DRAFT: &str = "nocode_publish.bad_draft";
/// An item the draft names is not available on the site it was designed on.
pub const MISSING_ITEM: &str = "nocode_publish.missing_item";
/// Fixture rows for a DocType the draft does not declare.
pub const DATA_NOT_DECLARED: &str = "nocode_publish.data_not_declared";
/// The generated `manifest.toml` does not pass `spot-manifest`.
pub const BAD_MANIFEST: &str = "nocode_publish.bad_manifest";
/// The packed bytes do not pass the validator the installer uses.
pub const BAD_BUNDLE: &str = "nocode_publish.bad_bundle";
/// The bundle asks for more than the host policy allows.
pub const OVER_PRIVILEGED: &str = "nocode_publish.over_privileged";
/// The captured customisation bundle is malformed.
pub const BAD_CUSTOM_BUNDLE: &str = "nocode_publish.bad_custom_bundle";
/// The review found a blocker, so the app cannot be listed.
pub const REVIEW_REJECTED: &str = "nocode_publish.review_rejected";
/// The same semantic version is already published.
pub const VERSION_EXISTS: &str = "nocode_publish.version_exists";
/// The new version does not exceed every published version.
pub const VERSION_NOT_INCREASING: &str = "nocode_publish.version_not_increasing";
/// A private release is published to an organisation that does not own it.
pub const PRIVATE_OTHER_ORGANISATION: &str = "nocode_publish.private_other_organisation";
/// The publisher signature is missing, does not verify, or names another identity.
pub const SIGNATURE: &str = "nocode_publish.signature";
/// The store registry refused the release.
pub const REGISTRY: &str = "nocode_publish.registry";
/// The install planner, executor or customisation apply refused the install.
pub const INSTALL: &str = "nocode_publish.install";
