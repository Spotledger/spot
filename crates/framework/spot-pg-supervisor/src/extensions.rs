//! First-start provisioning of the extensions the catalogue requires (ADR-087 sections
//! 4.3 and 11.3 S2).
//!
//! ADR-087 decided that PostGIS is required on every tier, the desktop edition included:
//! there is no built-in `point` fallback and no degraded mode. The desktop edition runs a
//! PostgreSQL bundled by the installer, so the supervisor is the component that creates
//! the extension once the cluster is up — and the component that fails the install when
//! it cannot.
//!
//! Two rules shape this module:
//!
//! - **The supervisor does not depend on `spot-pg-migrate`.** Installing an extension is
//!   the audited `extension.install` operation of that crate (dry run first, then the
//!   install bound to the dry run's plan hash). The supervisor only knows the *trait*
//!   [`ExtensionProvisioner`]; the host (the Windows service wrapper and its installer,
//!   WP-PRD-03) implements it over `spot-pg-migrate` and hands it in with
//!   [`Supervisor::with_extension_provisioner`](crate::supervisor::Supervisor::with_extension_provisioner).
//!   One trait, no crate dependency, no SQL in this crate (ADR-45).
//! - **There is no switch.** A provisioning failure is `Failed { reason }` and `Ready` is
//!   never published; no configuration value, environment variable or fallback turns it
//!   into a warning (ADR-087 section 4.7, principle 6).

use crate::error::SupervisorError;
use std::future::Future;
use std::pin::Pin;

/// The extension that makes a `geo` field storable, named once so the state reasons, the
/// events and the tests cannot drift apart.
pub const POSTGIS: &str = "postgis";

/// The extensions a bundled data directory must carry before it is reported usable.
///
/// One entry today. `btree_gist`, `pg_trgm` and `unaccent` are *trusted* extensions
/// (PostgreSQL F.8, read 27 Oct 2026) that a tenant migration role creates for itself when
/// a catalog needs them, so the supervisor does not provision them; `postgis` needs a
/// superuser and therefore the supervisor's own.
pub const REQUIRED_EXTENSIONS: [&str; 1] = [POSTGIS];

/// What one provisioning call did: the dry run, then the install bound to its plan hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionProvisioned {
    /// The extension that is now present.
    pub extension: String,
    /// The version installed, or already installed.
    pub version: String,
    /// The call found it present at an acceptable version and changed nothing: the
    /// idempotent path every later start takes.
    pub already_present: bool,
    /// The plan hash the install ran against (the dry run's), for the audit trail.
    pub plan_hash: String,
}

/// Why provisioning failed. The extension name and a reason text, nothing else: no
/// connection string, no password, no row value (ADR-087 section 4.3; ADR-44).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionFailure {
    /// The extension that could not be provisioned.
    pub extension: String,
    /// Why, in the host's words (for example the typed `migrate.*` message).
    pub reason: String,
}

impl ProvisionFailure {
    /// A failure for `extension` with `reason` as its explanation.
    pub fn new(extension: impl Into<String>, reason: impl Into<String>) -> Self {
        ProvisionFailure {
            extension: extension.into(),
            reason: reason.into(),
        }
    }
}

/// Creates a required extension through the platform's audited `extension.install`
/// operation.
///
/// The host implements this over `spot-pg-migrate`: the dry run
/// (`plan_extension_install`) first, then the real call (`apply_extension_install`)
/// carrying the dry run's plan hash. One call per extension, on every start: the platform
/// operation is idempotent — an extension present at an acceptable version is a no-op that
/// answers [`ExtensionProvisioned::already_present`], which is why a second start costs one
/// round trip and changes nothing.
///
/// The signature is spelled with a boxed future rather than `async fn` because the
/// supervisor holds `Arc<dyn ExtensionProvisioner>`: an `async fn` in a trait is not
/// dyn-compatible. An implementation reads the same as an `async fn` body inside
/// `Box::pin(async move { .. })`.
pub trait ExtensionProvisioner: Send + Sync {
    /// Dry run, then install `name` bound to the dry run's plan hash.
    fn provision<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ExtensionProvisioned, ProvisionFailure>> + Send + 'a>>;
}

/// Maps a provisioning failure to the typed error and the state reason. The reason names
/// PostGIS and carries the host's text unchanged; the supervisor adds nothing of its own.
pub(crate) fn unprovisionable(failure: &ProvisionFailure) -> SupervisorError {
    SupervisorError::PostgisUnprovisionable {
        extension: failure.extension.clone(),
        reason: failure.reason.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state reason names the extension even when the host's text does not.
    #[test]
    fn the_error_names_the_extension() {
        let error = unprovisionable(&ProvisionFailure::new(POSTGIS, "control file vanished"));
        let text = error.to_string();
        assert!(text.contains("postgis"), "{text}");
        assert!(text.contains("control file vanished"), "{text}");
    }

    /// `postgis` is the only extension the supervisor provisions (trusted ones are the
    /// migration role's business).
    #[test]
    fn only_postgis_is_required() {
        assert_eq!(REQUIRED_EXTENSIONS, ["postgis"]);
    }
}
