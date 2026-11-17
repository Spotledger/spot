//! The app runtime on a running server (WP-EXT-05.T3). The shared fixture host has no signed
//! sample app, no key to sign one with and no `AppsPort` adapter mounted yet (the host wiring of
//! `services/apps.rs` provides `dyn AppsPort` to the registry in a host task), so these cases
//! are named and **blocked**, each with the observation that proves the surface is missing.
//! They become `CaseDef::run` entries when the install path (WP-STR-04.T7 / management API) and
//! the `spot-http` adapter exist; the route itself is covered by `tests/routes_apps.rs`.

use super::CaseDef;

/// After the other groups; the cases own no fixture while blocked.
pub const ORDER: u32 = 40;

const SECTION: &str = "Apps";
const OWNER: &str = "WP-EXT-05.T3 (host adapter and install path)";
const NO_SURFACE: &str = "`GET /api/v1/apps` is not mounted by the host: no `AppsPort` adapter is provided and no signed app can be installed by the harness";

pub fn cases() -> Vec<CaseDef> {
    [
        ("apps: a_signed_sample_app_installs_and_runs_and_its_hook_effect_is_observable", "install, function call and hook effect observable; uninstall removes it from the list at once"),
        ("apps: a_timeout_kills_the_app_and_an_unrelated_request_stays_under_the_latency_bound", "the app is killed on timeout; the runtime and other requests are unaffected"),
        ("apps: a_tampered_bundle_or_a_revoked_publisher_is_failed_in_the_list_and_the_site_is_healthy", "an edited stored bundle after restart is `failed` with a code; the site stays healthy"),
        ("apps: the_network_capability_passes_the_egress_policy_and_loopback_metadata_and_rebinding_are_refused_and_audited", "an allow-listed host succeeds; 127.0.0.1, the metadata address and a DNS-rebinding stub are refused and audited"),
    ]
    .into_iter()
    .map(|(name, pins)| CaseDef::blocked(name, SECTION, pins, OWNER, NO_SURFACE))
    .collect()
}
