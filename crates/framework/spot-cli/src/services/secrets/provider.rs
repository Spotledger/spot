//! Choosing the key provider (WP-SEC-03.T16): `provider = "local"` is the key file, `provider =
//! "kms"` is `spot-secrets-kms` behind [`kms`](super::kms). Both end in the same
//! [`SecretsHost`]: a `KeyProvider` trait object and the vault sealed with it, so the host's
//! `open`, `verify_values` and the `secrets` commands no longer care which one it is.

use std::path::Path;
use std::sync::Arc;

use spot_core::{Clock, SystemClock};
use spot_egress::{EgressPolicy, Guard, Sender};
use spot_secrets::KeyProvider;
use spot_secrets_kms::{Entropy, OsEntropy};

use super::kms::{self, KmsSettings};
use super::{SecretsHost, Settings, codes, refuse};
use crate::Result;

/// What a KMS-backed host needs from the process that builds it.
///
/// [`Runtime::production`] is what `spot serve` and the commands use today. A host that has the
/// site's egress guard (`services::egress`) puts it in `guard`, so the KMS address is judged by
/// the site's policy file and every denial reaches the denial log.
#[derive(Clone)]
pub struct Runtime {
    /// The shared egress guard. With `None` the host judges the KMS address by a policy of its
    /// own that allows exactly the configured address for this site and nothing else.
    pub guard: Option<Arc<Guard>>,
    /// The sender to reach the KMS with; `None` sends through the guard's `HttpSender` (tests
    /// put a fake here, the guard still decides whether the address may be reached).
    pub sender: Option<Arc<dyn Sender>>,
    /// The source of `now` (cache lifetimes, probes).
    pub clock: Arc<dyn Clock>,
    /// The source of randomness (the blind-index seed on first use).
    pub entropy: Arc<dyn Entropy>,
}

impl Runtime {
    /// The system clock and the operating system's entropy, no shared guard.
    #[must_use]
    pub fn production() -> Self {
        Self {
            guard: None,
            sender: None,
            clock: Arc::new(SystemClock::new()),
            entropy: Arc::new(OsEntropy),
        }
    }

    /// The same runtime with the site's shared egress guard.
    #[must_use]
    pub fn with_guard(mut self, guard: Arc<Guard>) -> Self {
        self.guard = Some(guard);
        self
    }
}

/// A guard that lets `site` reach the configured KMS address and nothing else.
fn own_guard(settings: &KmsSettings, site: &str, clock: Arc<dyn Clock>) -> Result<Arc<Guard>> {
    let bad = |why: &str| {
        refuse(
            kms::codes::SETTINGS_INVALID,
            format!("services.secrets.kms.address {why}"),
        )
    };
    let url = url::Url::parse(&settings.address).map_err(|_| bad("is not a URL"))?;
    let host = url.host_str().ok_or_else(|| bad("has no host"))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let private = host
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .map(|ip| format!("allow_private = [\"{ip}\"]\n"))
        .unwrap_or_default();
    let text = format!(
        "[tenants.\"{site}\"]\n[[tenants.\"{site}\".allow]]\nscheme = \"https\"\nhost = \"{host}\"\nports = [{port}]\npurpose = \"connector\"\n{private}"
    );
    let policy =
        EgressPolicy::from_toml(&text).map_err(|_| bad("cannot be turned into a policy"))?;
    Ok(Arc::new(Guard::with_defaults(policy, clock)))
}

/// The host over `provider = "kms"`.
pub(super) async fn kms_host(
    settings: &Settings,
    root: &Path,
    site: &str,
    sites: usize,
    legacy: usize,
    runtime: &Runtime,
) -> Result<Arc<SecretsHost>> {
    let Some(kms_settings) = &settings.kms else {
        return Err(refuse(
            codes::KEK_UNSET,
            "services.secrets.provider is `kms` but [services.secrets.kms] is not set",
        ));
    };
    let files = kms_settings.for_site(root, site, sites)?;
    let guard = match &runtime.guard {
        Some(guard) => Arc::clone(guard),
        None => own_guard(kms_settings, site, Arc::clone(&runtime.clock))?,
    };
    let host = match &runtime.sender {
        Some(sender) => {
            kms::build_kms_provider_with(
                kms_settings,
                &files,
                guard,
                Arc::clone(sender),
                Arc::clone(&runtime.clock),
                Arc::clone(&runtime.entropy),
            )
            .await?
        }
        None => {
            kms::build_kms_provider(
                kms_settings,
                &files,
                guard,
                Arc::clone(&runtime.clock),
                Arc::clone(&runtime.entropy),
            )
            .await?
        }
    };
    let provider: Arc<dyn KeyProvider> = host.provider().clone();
    let secrets = SecretsHost {
        site: site.to_owned(),
        path: files.seed_file.clone(),
        provider,
        vault: host.vault(),
        local: None,
        kms: Some(host),
        legacy,
    };
    secrets.canary().await?;
    Ok(Arc::new(secrets))
}
