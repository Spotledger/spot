//! TLS options for the [`HttpSender`](crate::HttpSender): private trust anchors, a client
//! identity for mutual TLS, a minimum protocol version and server-name pinning.
//!
//! The egress policy ([`Guard`](crate::Guard)) decides *whether* a destination may be reached;
//! these options decide *how* the TLS session to it is set up. They never widen the policy:
//! address vetting, DNS pinning and redirect re-validation all happen before TLS.
//!
//! Everything is validated once, when a [`TlsOptions`] is built, and the rustls configurations
//! are built at the same moment, so a sender never builds one per request and a request never
//! alters one. No error text and no `Debug` output of any type here contains PEM text or key
//! bytes.

use crate::EgressError;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, DnsName, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, SupportedProtocolVersion, version as rustls_version};
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use zeroize::Zeroizing;

/// One or more PEM-encoded certificates (a trust anchor or a bundle of them).
///
/// Public material, but it still never appears in `Debug` output or in an error.
#[derive(Clone, PartialEq, Eq)]
pub struct CertPem(Vec<u8>);

impl CertPem {
    /// Wraps PEM text. Nothing is parsed here: [`TlsOptions::new`] validates it.
    pub fn new(pem: impl Into<Vec<u8>>) -> Self {
        Self(pem.into())
    }
}

impl fmt::Debug for CertPem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CertPem(<{} bytes>)", self.0.len())
    }
}

/// A client certificate chain and its private key, for mutual TLS.
///
/// The key's PEM bytes live in a zeroising buffer, are never cloned outside this type and
/// the rustls configuration built from it, are never logged, and are wiped when the value (and so
/// the [`TlsOptions`] holding it) is dropped.
///
/// The identity is offered to **one origin** only: the one given to
/// [`ClientIdentity::bound_to`], or, when none was given, the origin of the first HTTPS
/// request the sender makes. A redirect to any other origin is made without it. Use one sender
/// per peer, and prefer binding explicitly.
#[derive(Clone)]
pub struct ClientIdentity {
    chain_pem: Vec<u8>,
    key_pem: Zeroizing<Vec<u8>>,
    bound_to: Option<PeerOrigin>,
}

impl ClientIdentity {
    /// A chain (end-entity certificate first) and the matching private key, both PEM. Nothing
    /// is parsed here: [`TlsOptions::new`] validates it.
    pub fn from_pem(chain_pem: impl Into<Vec<u8>>, key_pem: impl Into<Vec<u8>>) -> Self {
        Self {
            chain_pem: chain_pem.into(),
            key_pem: Zeroizing::new(key_pem.into()),
            bound_to: None,
        }
    }

    /// Offers this identity only to `host:port` (the host as written in the request URL,
    /// compared case-insensitively).
    #[must_use]
    pub fn bound_to(mut self, host: &str, port: u16) -> Self {
        self.bound_to = Some(PeerOrigin {
            host: host.to_ascii_lowercase(),
            port,
        });
        self
    }
}

impl fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("chain", &format_args!("<{} bytes>", self.chain_pem.len()))
            .field("key", &format_args!("<redacted>"))
            .field("bound_to", &self.bound_to)
            .finish()
    }
}

/// Which certificates a server must chain to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TrustMode {
    /// The public web roots only (today's behaviour).
    #[default]
    PublicRoots,
    /// The public web roots plus the given anchors (a peer behind a private CA that is also
    /// allowed to be a public one).
    PublicPlusPem(Vec<CertPem>),
    /// Only the given anchors, no public root: the pinned-anchor mode for a peer the operator
    /// runs. A server signed by any other CA is refused, however public that CA is.
    OnlyPem(Vec<CertPem>),
}

/// The lowest TLS protocol version a connection may negotiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsVersion {
    /// TLS 1.2 and 1.3 (the default).
    #[default]
    Tls12,
    /// TLS 1.3 only.
    Tls13,
}

/// An origin as the sender compares it: lower-case host and port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerOrigin {
    host: String,
    port: u16,
}

/// A secret-free description of the options a sender runs with (for diagnostics and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsSummary {
    /// `"public_roots"`, `"public_plus_pem"` or `"only_pem"`.
    pub trust: &'static str,
    /// Whether a client identity is configured.
    pub client_identity: bool,
    /// The minimum protocol version.
    pub min_version: TlsVersion,
    /// Whether a server-name override is configured.
    pub server_name_override: bool,
}

/// The TLS options of an [`HttpSender`](crate::HttpSender). Build one with [`TlsOptions::new`]
/// (everything is validated there) or [`TlsOptions::public_roots`] (today's default).
pub struct TlsOptions {
    trust: TrustMode,
    client_identity: Option<ClientIdentity>,
    min_version: TlsVersion,
    server_name_override: Option<String>,
    configs: Configs,
}

/// The configurations built from the options, once.
#[derive(Clone)]
struct Configs {
    /// For the peer origin: carries the client identity when there is one.
    peer: Arc<ClientConfig>,
    /// For any other origin: the same trust and versions, never a client identity.
    other: Arc<ClientConfig>,
}

impl TlsOptions {
    /// Validates and builds the options.
    ///
    /// Fails with `egress.invalid_tls_options` when: a trust PEM does not parse or holds no
    /// certificate, an anchor list is empty, the client chain is empty or does not parse, the
    /// key does not parse or does not match the chain's end-entity certificate, or the server-name
    /// override is not an exact DNS name (a wildcard, an IP address or an empty string is
    /// refused). The error text never contains PEM content.
    pub fn new(
        trust: TrustMode,
        client_identity: Option<ClientIdentity>,
        min_version: TlsVersion,
        server_name_override: Option<String>,
    ) -> Result<Self, EgressError> {
        let server_name_override = server_name_override
            .map(|name| validate_server_name(&name))
            .transpose()?;
        let roots = roots_for(&trust)?;
        let other = build_config(min_version, roots.clone(), None)?;
        let (peer, identity) = match client_identity {
            Some(identity) => {
                let parsed = parse_identity(&identity)?;
                (
                    build_config(min_version, roots, Some(parsed))?,
                    Some(identity),
                )
            }
            None => (other.clone(), None),
        };
        Ok(Self {
            trust,
            client_identity: identity,
            min_version,
            server_name_override,
            configs: Configs {
                peer: Arc::new(peer),
                other: Arc::new(other),
            },
        })
    }

    /// Today's behaviour: the public web roots, no client identity, TLS 1.2 or later, no name
    /// override. Cannot fail in practice (the inputs are fixed); the `Result` is kept so no
    /// panic is possible if the TLS library ever rejects them.
    pub fn public_roots() -> Result<Self, EgressError> {
        Self::new(TrustMode::PublicRoots, None, TlsVersion::Tls12, None)
    }

    /// The trust mode.
    pub fn trust(&self) -> &TrustMode {
        &self.trust
    }

    /// The client identity, if any.
    pub fn client_identity(&self) -> Option<&ClientIdentity> {
        self.client_identity.as_ref()
    }

    /// The minimum protocol version.
    pub fn min_version(&self) -> TlsVersion {
        self.min_version
    }

    /// The server-name override (lower-case), if any.
    pub fn server_name_override(&self) -> Option<&str> {
        self.server_name_override.as_deref()
    }

    /// A secret-free summary.
    pub fn summary(&self) -> TlsSummary {
        TlsSummary {
            trust: match self.trust {
                TrustMode::PublicRoots => "public_roots",
                TrustMode::PublicPlusPem(_) => "public_plus_pem",
                TrustMode::OnlyPem(_) => "only_pem",
            },
            client_identity: self.client_identity.is_some(),
            min_version: self.min_version,
            server_name_override: self.server_name_override.is_some(),
        }
    }
}

impl fmt::Debug for TlsOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsOptions")
            .field("trust", &self.trust)
            .field("client_identity", &self.client_identity)
            .field("min_version", &self.min_version)
            .field("server_name_override", &self.server_name_override)
            .finish_non_exhaustive()
    }
}

/// What the sender keeps of a [`TlsOptions`]: the built configurations, the peer binding and the
/// override. The PEM and key buffers are not kept (they are wiped when the options drop).
pub(crate) struct TlsRuntime {
    configs: Configs,
    summary: TlsSummary,
    server_name_override: Option<String>,
    /// Whether a peer-specific setting exists (identity or override): only then is an origin bound.
    peer_specific: bool,
    peer: OnceLock<PeerOrigin>,
}

/// The choice made for one HTTPS connection.
pub(crate) struct Selection {
    pub(crate) config: Arc<ClientConfig>,
    /// The name the certificate is verified against.
    pub(crate) server_name: String,
    /// The override that was applied, for the denial record.
    pub(crate) override_used: Option<String>,
}

impl TlsRuntime {
    pub(crate) fn from_options(options: TlsOptions) -> Self {
        let peer_specific =
            options.client_identity.is_some() || options.server_name_override.is_some();
        let peer = OnceLock::new();
        if let Some(origin) = options
            .client_identity
            .as_ref()
            .and_then(|identity| identity.bound_to.clone())
        {
            let _ = peer.set(origin);
        }
        let summary = options.summary();
        Self {
            configs: options.configs.clone(),
            summary,
            server_name_override: options.server_name_override.clone(),
            peer_specific,
            peer,
        }
    }

    pub(crate) fn summary(&self) -> TlsSummary {
        self.summary
    }

    /// Picks the configuration and the verification name for a connection to `host:port` (the
    /// original host of the vetted request). The first HTTPS origin binds the peer when nothing
    /// bound it earlier; any other origin gets the identity-free configuration and its own host
    /// name.
    pub(crate) fn select(&self, host: &str, port: u16) -> Selection {
        let host = host.to_ascii_lowercase();
        if !self.peer_specific {
            return Selection {
                config: self.configs.other.clone(),
                server_name: host,
                override_used: None,
            };
        }
        let bound = self.peer.get_or_init(|| PeerOrigin {
            host: host.clone(),
            port,
        });
        if bound.host == host && bound.port == port {
            Selection {
                config: self.configs.peer.clone(),
                server_name: self
                    .server_name_override
                    .clone()
                    .unwrap_or_else(|| host.clone()),
                override_used: self.server_name_override.clone(),
            }
        } else {
            Selection {
                config: self.configs.other.clone(),
                server_name: host,
                override_used: None,
            }
        }
    }
}

fn invalid(reason: &str) -> EgressError {
    EgressError::InvalidTlsOptions(reason.to_owned())
}

/// The ring provider, chosen explicitly so the build never depends on a process-default one.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

static VERSIONS_12: [&SupportedProtocolVersion; 2] =
    [&rustls_version::TLS12, &rustls_version::TLS13];
static VERSIONS_13: [&SupportedProtocolVersion; 1] = [&rustls_version::TLS13];

fn versions(min: TlsVersion) -> &'static [&'static SupportedProtocolVersion] {
    match min {
        TlsVersion::Tls12 => &VERSIONS_12,
        TlsVersion::Tls13 => &VERSIONS_13,
    }
}

type Identity = (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>);

fn build_config(
    min: TlsVersion,
    roots: RootCertStore,
    identity: Option<Identity>,
) -> Result<ClientConfig, EgressError> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(versions(min))
        .map_err(|_| invalid("the TLS protocol versions were rejected"))?
        .with_root_certificates(roots);
    match identity {
        Some((chain, key)) => builder
            .with_client_auth_cert(chain, key)
            .map_err(|_| invalid("the client key is unusable or does not match the certificate")),
        None => Ok(builder.with_no_client_auth()),
    }
}

fn roots_for(trust: &TrustMode) -> Result<RootCertStore, EgressError> {
    let mut roots = RootCertStore::empty();
    let pems: &[CertPem] = match trust {
        TrustMode::PublicRoots => {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            return Ok(roots);
        }
        TrustMode::PublicPlusPem(pems) => {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            pems
        }
        TrustMode::OnlyPem(pems) => pems,
    };
    if pems.is_empty() {
        return Err(invalid("no trust anchor was given"));
    }
    for pem in pems {
        let mut found = 0usize;
        for item in CertificateDer::pem_slice_iter(&pem.0) {
            let der = item.map_err(|_| invalid("a trust anchor is not valid PEM"))?;
            roots
                .add(der)
                .map_err(|_| invalid("a trust anchor is not a usable certificate"))?;
            found += 1;
        }
        if found == 0 {
            return Err(invalid("a trust anchor PEM holds no certificate"));
        }
    }
    Ok(roots)
}

fn parse_identity(identity: &ClientIdentity) -> Result<Identity, EgressError> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&identity.chain_pem)
        .collect::<Result<_, _>>()
        .map_err(|_| invalid("the client certificate chain is not valid PEM"))?;
    if chain.is_empty() {
        return Err(invalid("the client certificate chain is empty"));
    }
    let key = PrivateKeyDer::from_pem_slice(identity.key_pem.as_slice())
        .map_err(|_| invalid("the client private key is not valid PEM"))?;
    Ok((chain, key))
}

/// An override must be one exact DNS name: no wildcard, no IP address, nothing empty.
fn validate_server_name(name: &str) -> Result<String, EgressError> {
    if name.is_empty() {
        return Err(invalid("the server name override is empty"));
    }
    if name.contains('*') {
        return Err(invalid("the server name override must not be a wildcard"));
    }
    if name.parse::<IpAddr>().is_ok() {
        return Err(invalid("the server name override must be a DNS name"));
    }
    let lower = name.to_ascii_lowercase();
    DnsName::try_from(lower.as_str())
        .map_err(|_| invalid("the server name override is not a valid DNS name"))?;
    Ok(lower)
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn default_options_are_todays_behaviour() {
        let options = TlsOptions::public_roots().unwrap();
        assert_eq!(options.trust(), &TrustMode::PublicRoots);
        assert!(options.client_identity().is_none());
        assert_eq!(options.min_version(), TlsVersion::Tls12);
        assert!(options.server_name_override().is_none());
        assert!(!options.configs.peer.client_auth_cert_resolver.has_certs());
    }

    #[test]
    fn server_name_must_be_an_exact_dns_name() {
        for bad in [
            "",
            "*.example.com",
            "*",
            "127.0.0.1",
            "::1",
            "bad name",
            "a..b",
        ] {
            assert!(validate_server_name(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            validate_server_name("Peer.Example.COM").unwrap(),
            "peer.example.com"
        );
    }

    #[test]
    fn empty_anchor_lists_are_refused() {
        for trust in [
            TrustMode::OnlyPem(Vec::new()),
            TrustMode::PublicPlusPem(Vec::new()),
        ] {
            let err = TlsOptions::new(trust, None, TlsVersion::Tls12, None).unwrap_err();
            assert_eq!(err.code(), "egress.invalid_tls_options");
        }
    }

    #[test]
    fn runtime_without_peer_settings_never_binds() {
        let runtime = TlsRuntime::from_options(TlsOptions::public_roots().unwrap());
        let a = runtime.select("A.test", 443);
        assert_eq!(a.server_name, "a.test");
        assert!(runtime.peer.get().is_none());
    }
}
