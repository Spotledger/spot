//! The mutual-TLS fetcher `spot licence renew` uses outside a running site (WP-CTL-04.T3).
//!
//! A running site gets its fetcher through [`super::LicencePorts`] from the host's shared egress
//! guard. The command has no site: it builds a guard of its own whose policy allows exactly the
//! configured control-plane `host:port` (as `services/mail.rs` does for its SMTP host) and nothing
//! else, then a sender with the host's certificate, key and CA pinned (`OnlyPem`: the control
//! plane's CA is the only trust anchor, the public roots are not consulted).
//!
//! The one line that depends on the TLS options of `spot-egress` (WP-SEC-09.T8) is
//! [`build_sender`]; its names are those of that task's brief and are the only place to adjust if
//! the merged API spells a constructor differently.

use std::sync::Arc;
use std::time::Duration;

use spot_core::Clock;
use spot_egress::{
    AllowEntry, EgressConnector, EgressPolicy, Guard, HttpSender, InMemorySink, Purpose, Sender,
    SystemResolver, TenantPolicy,
};

use super::host::HostError;
use super::renew::{EgressFetcher, LicenceFetcher};
use super::{Settings, check_credentials, code};

/// The egress policy of the renewal: the control plane's `host:port`, https only.
fn policy_for(site: &str, host: &str, port: u16) -> EgressPolicy {
    let entry = AllowEntry {
        scheme: Some("https".to_owned()),
        host: host.to_owned(),
        ports: Some(vec![port]),
        purpose: Some(Purpose::Connector.as_str().to_owned()),
        allow_private: host
            .parse::<std::net::IpAddr>()
            .ok()
            .map(|ip| vec![ip.to_string()]),
    };
    let mut policy = EgressPolicy {
        default: "deny_non_public".to_owned(),
        dangerous_ports: vec![22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250],
        tenants: std::collections::HashMap::new(),
    };
    policy.tenants.insert(
        site.to_owned(),
        TenantPolicy {
            allow: vec![entry],
            ..TenantPolicy::default()
        },
    );
    policy
}

fn read_pem(path: &str, name: &str) -> Result<Vec<u8>, HostError> {
    std::fs::read(path).map_err(|_| {
        HostError::new(
            code::FILE_UNSAFE,
            format!("services.licence.{name} ({path}) cannot be read"),
        )
    })
}

/// The sender with the host identity and the pinned control-plane CA.
fn build_sender(
    connector: EgressConnector,
    settings: &Settings,
) -> Result<Arc<dyn Sender>, HostError> {
    use spot_egress::{CertPem, ClientIdentity, TlsOptions, TlsVersion, TrustMode};
    let refuse = |name: &str| {
        HostError::new(
            code::INVALID_SETTING,
            format!("services.licence.{name} is not a usable PEM file"),
        )
    };
    let ca = CertPem::new(read_pem(&settings.ca_file, "ca_file")?);
    let identity = ClientIdentity::from_pem(
        read_pem(&settings.host_cert, "host_cert")?,
        read_pem(&settings.host_key, "host_key")?,
    );
    let options = TlsOptions::new(
        TrustMode::OnlyPem(vec![ca]),
        Some(identity),
        TlsVersion::Tls12,
        None,
    )
    .map_err(|_| refuse("host_key"))?;
    Ok(Arc::new(HttpSender::with_tls(
        connector,
        Duration::from_secs(30),
        options,
    )))
}

/// A fetcher for `settings.control_plane`, for the site called `site`.
///
/// # Errors
/// A [`HostError`]: a bad setting, unsafe or unusable credential files.
pub fn fetcher(
    settings: &Settings,
    site: &str,
    clock: Arc<dyn Clock>,
) -> Result<Arc<dyn LicenceFetcher>, HostError> {
    let (host, port) = settings.control_plane_endpoint()?;
    check_credentials(settings).map_err(|m| HostError::new(code::FILE_UNSAFE, m))?;
    let guard = Arc::new(Guard::new(
        policy_for(site, &host, port),
        Arc::new(SystemResolver),
        Arc::new(InMemorySink::new()),
        clock,
    ));
    let connector = || EgressConnector::new(Arc::clone(&guard), site, Purpose::Connector);
    let sender = build_sender(connector(), settings)?;
    Ok(Arc::new(EgressFetcher::new(
        connector(),
        sender,
        &settings.control_plane,
    )?))
}
