//! The stable rule ids every review finding carries.
//!
//! A finding's `rule` is either one of these `app_review.*` ids (this crate's own rules) or a
//! `bundle.*` code passed through from `spot-bundle` (`Bundle::open` and
//! `check_privileges`). Every id here has one entry in
//! `crates/framework/spot-errors/catalogue/app_review.toml`; the catalogue file name is the
//! id's first segment, as `spot-errors` requires.
//!
//! The ids are grouped by the stage that raises them; the [`crate::Severity`] and `risk_id`
//! each finding carries are decided at the raise site (see the stage modules).

// --- static checks -------------------------------------------------------------------------

/// A capability the manifest declares that nothing in the component exercises.
pub const STATIC_UNUSED_CAPABILITY: &str = "app_review.static.unused_capability";
/// The manifest has no human title.
pub const STATIC_TITLE_MISSING: &str = "app_review.static.title_missing";
/// A patch omits its `reversible` or `idempotent` declaration.
pub const STATIC_PATCH_UNDECLARED: &str = "app_review.static.patch_undeclared";
/// A data or schema patch is neither reversible nor idempotent.
pub const STATIC_PATCH_IRREVERSIBLE: &str = "app_review.static.patch_irreversible";
/// The `requires_host` requirement sets no upper bound.
pub const STATIC_REQUIRES_HOST_OPEN: &str = "app_review.static.requires_host_open";

// --- security scan -------------------------------------------------------------------------

/// A declared network endpoint whose host is a private address.
pub const SCAN_ENDPOINT_PRIVATE: &str = "app_review.scan.endpoint.private";
/// A declared network endpoint whose host is the loopback address.
pub const SCAN_ENDPOINT_LOOPBACK: &str = "app_review.scan.endpoint.loopback";
/// A declared network endpoint whose host is a link-local address.
pub const SCAN_ENDPOINT_LINK_LOCAL: &str = "app_review.scan.endpoint.link_local";
/// A declared network endpoint whose host is a cloud metadata address.
pub const SCAN_ENDPOINT_METADATA: &str = "app_review.scan.endpoint.metadata";
/// A declared network endpoint whose host is a raw IP address.
pub const SCAN_ENDPOINT_RAW_IP: &str = "app_review.scan.endpoint.raw_ip";
/// Write access to a platform DocType.
pub const SCAN_DOCTYPE_PLATFORM_WRITE: &str = "app_review.scan.doctype.platform_write";
/// Read or write access to more DocTypes than the configuration allows.
pub const SCAN_DOCTYPE_BROAD: &str = "app_review.scan.doctype.broad";
/// `files = read_write` with nothing in the component importing a file function.
pub const SCAN_FILES_READ_WRITE_UNUSED: &str = "app_review.scan.files.read_write_unused";
/// The e-mail capability with no declared sender purpose.
pub const SCAN_EMAIL_UNDECLARED_PURPOSE: &str = "app_review.scan.email.undeclared_purpose";
/// An asset with an executable or script extension.
pub const SCAN_ASSET_EXECUTABLE: &str = "app_review.scan.asset.executable";
/// An HTML asset carrying active content (a remote script, inline `eval(`, `document.cookie`
/// or an absolute-URL `fetch(`).
pub const SCAN_ASSET_ACTIVE_CONTENT: &str = "app_review.scan.asset.active_content";
/// The component or an asset carries bytes that look like an embedded key or token.
pub const SCAN_SECRET_EMBEDDED: &str = "app_review.scan.secret.embedded";

// --- resource profile ----------------------------------------------------------------------

/// A call ran out of fuel.
pub const RESOURCE_FUEL: &str = "app_review.resource.fuel";
/// A call grew past its memory limit.
pub const RESOURCE_MEMORY: &str = "app_review.resource.memory";
/// A call made more host calls than its limit.
pub const RESOURCE_HOST_CALLS: &str = "app_review.resource.host_calls";
/// A call ran past its wall-clock limit.
pub const RESOURCE_TIMEOUT: &str = "app_review.resource.timeout";
/// A call trapped.
pub const RESOURCE_TRAP: &str = "app_review.resource.trap";
/// A call produced a result larger than its output limit.
pub const RESOURCE_OUTPUT_LIMIT: &str = "app_review.resource.output_limit";
/// A call produced a result above the configured share of its output limit.
pub const RESOURCE_OUTPUT_SHARE: &str = "app_review.resource.output_share";
/// The component does not export the entry the profile tried to call.
pub const RESOURCE_ENTRY_MISSING: &str = "app_review.resource.entry_missing";
/// The entry rejected the arguments the profile passed.
pub const RESOURCE_BAD_ARGUMENTS: &str = "app_review.resource.bad_arguments";
/// The component could not be compiled, linked or instantiated under the host.
pub const RESOURCE_INSTANTIATE_FAILED: &str = "app_review.resource.instantiate_failed";
/// A call asked for a capability the manifest did not declare.
pub const RESOURCE_CAPABILITY_DENIED: &str = "app_review.resource.capability_denied";
/// The author's `profile.toml` does not parse; the review fell back to the default entry.
pub const RESOURCE_PROFILE_INVALID: &str = "app_review.resource.profile_invalid";

// --- contract probe ------------------------------------------------------------------------

/// A contract probe (DocType metadata, permissions, fixtures) failed.
pub const PROBE_CONTRACT: &str = "app_review.probe.contract";

// --- risk ids cited by the rules -----------------------------------------------------------

/// Risk R-023: a tenant administrator approves capabilities they do not understand.
pub const RISK_OVER_PRIVILEGE: &str = "R-023";
/// Risk R-024: an app burns CPU or memory, or blocks others.
pub const RISK_RESOURCE: &str = "R-024";
/// Risk R-025: a publisher ships a backdoored update or a broken one.
pub const RISK_PUBLISHER: &str = "R-025";
/// Risk R-030: outbound calls to internal addresses or attacker hosts.
pub const RISK_EGRESS: &str = "R-030";
/// Risk R-041: hostile upload or active content served from the site origin.
pub const RISK_ACTIVE_CONTENT: &str = "R-041";
