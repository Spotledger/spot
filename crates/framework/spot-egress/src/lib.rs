#![doc = include_str!("../README.md")]
#![allow(clippy::collapsible_if, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use thiserror::Error;
use url::Url;

#[allow(missing_docs)]
mod address;
#[allow(missing_docs)]
mod connector;
#[allow(missing_docs)]
mod denial;
#[allow(missing_docs)]
mod policy;
#[allow(missing_docs)]
mod resolver;
#[allow(missing_docs)]
mod sender;

pub use address::{AddrClass, classify};
pub use connector::{EgressConnector, is_sensitive_header};
pub use denial::{DenialRecord, DenialSink, InMemorySink, RateLimitedSink};
pub use hyper::body::Bytes;
pub use policy::{AllowEntry, DenyEntry, EgressPolicy, TenantPolicy};
pub use resolver::{Resolver, SystemResolver, TestResolver};
pub use sender::{HttpRequest, HttpSender, MAX_REQUEST_BODY_BYTES, Method, Response, Sender};

use spot_core::Clock;

/// A request to authorize an outbound connection.
#[derive(Debug, Clone)]
pub struct Request {
    /// The target URL (must be http or https).
    pub url: Url,
    /// The purpose of the request.
    pub purpose: Purpose,
    /// The tenant making the request.
    pub tenant: String,
    /// The actor (user or service) making the request.
    pub actor: Option<String>,
}

/// The purpose of an outbound request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purpose {
    /// A webhook invocation.
    Webhook,
    /// A script invocation.
    Script,
    /// A connector (third-party integration).
    Connector,
    /// Mail sending.
    Mail,
    /// LLM provider call.
    Llm,
    /// Template rendering.
    Template,
    /// PDF rendering.
    Pdf,
    /// App-specific outbound call.
    App(u64),
}

impl Purpose {
    /// Get the string representation of this purpose.
    pub fn as_str(&self) -> &'static str {
        match self {
            Purpose::Webhook => "webhook",
            Purpose::Script => "script",
            Purpose::Connector => "connector",
            Purpose::Mail => "mail",
            Purpose::Llm => "llm",
            Purpose::Template => "template",
            Purpose::Pdf => "pdf",
            Purpose::App(_) => "app",
        }
    }
}

/// An authorized set of addresses that have been vetted for a request.
///
/// **Connect only to these addresses.** The host was resolved once and every answer checked
/// against the policy; a caller that resolves the hostname again (a plain `connect(host, port)`,
/// a client library's own lookup) can be handed a different, internal address by a hostile DNS
/// server (DNS rebinding) and so defeats the guard. Dial [`Authorized::socket_addrs`], and use
/// [`Authorized::host`] only as the TLS server name and the `Host` header.
#[derive(Debug, Clone)]
pub struct Authorized {
    /// The pinned IP addresses to connect to (never connect to hostnames).
    pub addrs: Vec<SocketAddr>,
    /// The original hostname (for SNI and the Host header).
    pub original_host: String,
}

impl Authorized {
    /// The resolved and checked socket addresses (port included) to connect to, and only to.
    /// Never resolve the hostname again after authorising: see the type documentation.
    pub fn socket_addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// The hostname that was authorised: use it for the TLS server name and the `Host` header,
    /// never as the address to dial.
    pub fn host(&self) -> &str {
        &self.original_host
    }
}

/// Result of address authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthResult {
    /// The request is allowed.
    Allowed,
    /// The address is denied.
    DeniedAddress(AddrClass),
    /// The policy denies the request.
    DeniedPolicy,
    /// The port is denied.
    DeniedPort,
    /// Resolution returned mixed results (possible attack).
    ResolutionMixed,
}

/// Error types for egress operations.
#[derive(Debug, Error, Clone)]
pub enum EgressError {
    /// A resolved address was not allowed.
    #[error("address not allowed: {class:?}")]
    AddressDenied {
        /// The address class that was denied.
        class: AddrClass,
    },

    /// The policy does not allow this destination.
    #[error("connection denied by policy")]
    PolicyDenied,

    /// The port is not allowed.
    #[error("port not allowed: {port}")]
    PortDenied {
        /// The denied port.
        port: u16,
    },

    /// DNS rebinding detected (mixed resolution results).
    #[error("dns rebinding detected")]
    ResolutionMixed,

    /// A redirect was not allowed.
    #[error("redirect denied")]
    RedirectDenied,

    /// Too many redirects.
    #[error("too many redirects")]
    TooManyRedirects,

    /// Response body is too large.
    #[error("response body too large")]
    ResponseTooLarge,

    /// The request body is larger than the sender permits.
    #[error("request body too large")]
    RequestTooLarge {
        /// The maximum permitted body size, in bytes.
        limit: u64,
    },

    /// The request URL is invalid.
    #[error("invalid url")]
    InvalidUrl,

    /// DNS resolution failed.
    #[error("resolution failed")]
    ResolutionFailed,

    /// Configuration is invalid.
    #[error("invalid configuration")]
    InvalidConfig(
        /// The error message describing the configuration problem.
        String,
    ),
}

impl EgressError {
    /// The stable catalogue code (`egress.*`). The text a tenant sees never names which internal
    /// address matched; the admin-visible denial record carries the detail.
    pub fn code(&self) -> &'static str {
        match self {
            Self::AddressDenied { .. } => "egress.denied_address",
            Self::PolicyDenied => "egress.denied_by_policy",
            Self::PortDenied { .. } => "egress.denied_port",
            Self::ResolutionMixed => "egress.resolution_mixed",
            Self::RedirectDenied => "egress.redirect_denied",
            Self::TooManyRedirects => "egress.too_many_redirects",
            Self::ResponseTooLarge => "egress.response_too_large",
            Self::RequestTooLarge { .. } => "egress.request_too_large",
            Self::InvalidUrl => "egress.invalid_url",
            Self::ResolutionFailed => "egress.resolution_failed",
            Self::InvalidConfig(_) => "egress.invalid_config",
        }
    }
}

/// The scheme recorded for, and matched against allow entries by, address-based requests.
const GENERIC_SCHEME: &str = "generic";

/// What one authorization decides on.
struct Destination<'a> {
    scheme: &'a str,
    host: &'a str,
    port: u16,
    purpose: Purpose,
    tenant: &'a str,
    actor: Option<String>,
}

/// The core Guard that authorizes outbound connections.
#[allow(missing_docs)]
pub struct Guard {
    /// The egress policy (public for testing).
    pub policy: EgressPolicy,
    resolver: Arc<dyn Resolver>,
    sink: Arc<dyn DenialSink>,
    clock: Arc<dyn Clock>,
}

impl Guard {
    /// Create a new Guard with custom resolver and sink.
    pub fn new(
        policy: EgressPolicy,
        resolver: Arc<dyn Resolver>,
        sink: Arc<dyn DenialSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            policy,
            resolver,
            sink,
            clock,
        }
    }

    /// Create a new Guard with default resolver and in-memory sink.
    pub fn with_defaults(policy: EgressPolicy, clock: Arc<dyn Clock>) -> Self {
        let sink = Arc::new(InMemorySink::new());
        let resolver = Arc::new(SystemResolver);
        Self::new(policy, resolver, sink, clock)
    }

    /// Authorize a request for outbound connection.
    pub async fn authorize(&self, req: &Request) -> Result<Authorized, EgressError> {
        // Parse the URL and validate scheme
        let scheme = req.url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(EgressError::InvalidUrl);
        }

        // Extract host and port
        let host = req.url.host_str().ok_or(EgressError::InvalidUrl)?;
        let port = req
            .url
            .port()
            .unwrap_or(if scheme == "https" { 443 } else { 80 });

        self.vet(
            Destination {
                scheme,
                host,
                port,
                purpose: req.purpose,
                tenant: &req.tenant,
                actor: req.actor.clone(),
            },
            false,
        )
        .await
    }

    /// Authorize an address-based request (for non-HTTP callers like SMTP, DB).
    ///
    /// The decision is the one [`Guard::authorize`] makes for a URL with the same host, port and
    /// purpose, through the same code: the tenant's deny list wins, an allow entry has to cover
    /// the host, port and purpose (an entry without a `scheme` covers it; one that names `http`
    /// or `https` does not), a non-public address is reachable only through an `allow_private`
    /// entry of that same entry (cloud metadata never), and a dangerous port only when the
    /// entry lists it. Every denial is recorded with the tenant, actor and purpose. The one
    /// difference: a tenant with **no entry at all** gets the default policy as written
    /// (`deny_non_public`: public addresses on ordinary ports pass, nothing else does), where the
    /// URL path denies it outright.
    ///
    /// Returns the pinned addresses to connect to; connect to those only
    /// ([`Authorized::socket_addrs`]), never to the hostname again.
    pub async fn authorize_addr(
        &self,
        host: &str,
        port: u16,
        purpose: Purpose,
        tenant: &str,
        actor: Option<String>,
    ) -> Result<Authorized, EgressError> {
        self.vet(
            Destination {
                scheme: GENERIC_SCHEME,
                host,
                port,
                purpose,
                tenant,
                actor,
            },
            true,
        )
        .await
    }

    /// The one decision both entry points make: policy, resolve, classify every answer, port.
    /// `unlisted_tenant_default` lets a tenant with no policy entry fall through to the default
    /// policy's address and port rules instead of the allow-list gate.
    async fn vet(
        &self,
        dest: Destination<'_>,
        unlisted_tenant_default: bool,
    ) -> Result<Authorized, EgressError> {
        let Destination {
            scheme,
            host,
            port,
            purpose,
            tenant,
            actor,
        } = dest;
        let denial = |rule: &str, resolved: Vec<std::net::IpAddr>, class: AddrClass| {
            self.sink.denied(DenialRecord {
                tenant: tenant.to_string(),
                purpose,
                actor: actor.clone(),
                scheme: scheme.to_string(),
                host: host.to_string(),
                port,
                resolved,
                class,
                rule: rule.to_string(),
                at: self.clock.now_utc(),
            });
        };

        // Check policy for this tenant and purpose
        let gated = !(unlisted_tenant_default && !self.policy.has_tenant(tenant));
        if gated && !self.policy.is_allowed(tenant, purpose, scheme, host, port) {
            denial("policy", vec![], AddrClass::Public);
            return Err(EgressError::PolicyDenied);
        }

        // Resolve the host
        let addrs = match self.resolver.resolve(host).await {
            Ok(addrs) => addrs,
            Err(_) => {
                denial("resolution", vec![], AddrClass::Other);
                return Err(EgressError::ResolutionFailed);
            }
        };

        if addrs.is_empty() {
            denial("resolution", vec![], AddrClass::Other);
            return Err(EgressError::ResolutionFailed);
        }

        // Classify all addresses: one denied answer refuses the whole host
        for addr in &addrs {
            let class = classify(*addr);
            if !self
                .policy
                .is_address_allowed_for(tenant, purpose, scheme, host, *addr, port, class)
            {
                denial("address", addrs.clone(), class);
                return Err(EgressError::AddressDenied { class });
            }
        }

        // Validate port
        if !self
            .policy
            .is_port_allowed_for(tenant, purpose, scheme, host, port)
        {
            denial("port", addrs, AddrClass::Public);
            return Err(EgressError::PortDenied { port });
        }

        Ok(Authorized {
            addrs: addrs
                .into_iter()
                .map(|addr| SocketAddr::new(addr, port))
                .collect(),
            original_host: host.to_string(),
        })
    }

    /// Create a guard for webhook requests (convenience method).
    pub fn for_webhook(
        policy: EgressPolicy,
        resolver: Arc<dyn Resolver>,
        sink: Arc<dyn DenialSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new(policy, resolver, sink, clock)
    }

    /// Create a guard for script requests (convenience method).
    pub fn for_script(
        policy: EgressPolicy,
        resolver: Arc<dyn Resolver>,
        sink: Arc<dyn DenialSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new(policy, resolver, sink, clock)
    }

    /// Create a guard for LLM requests (convenience method).
    pub fn for_llm(
        policy: EgressPolicy,
        resolver: Arc<dyn Resolver>,
        sink: Arc<dyn DenialSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new(policy, resolver, sink, clock)
    }

    /// Authorize a URI for a specific purpose (for tower connectors).
    ///
    /// Returns the pinned addresses to connect to.
    pub async fn authorize_uri(
        &self,
        uri: &http::Uri,
        purpose: Purpose,
        tenant: &str,
        actor: Option<String>,
    ) -> Result<Authorized, EgressError> {
        let url = uri
            .to_string()
            .parse()
            .map_err(|_| EgressError::InvalidUrl)?;

        let request = Request {
            url,
            purpose,
            tenant: tenant.to_string(),
            actor,
        };

        self.authorize(&request).await
    }
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_purpose_as_str() {
        assert_eq!(Purpose::Webhook.as_str(), "webhook");
        assert_eq!(Purpose::Script.as_str(), "script");
        assert_eq!(Purpose::Mail.as_str(), "mail");
        assert_eq!(Purpose::Llm.as_str(), "llm");
    }

    #[test]
    fn test_denial_record_no_secrets() {
        // Create a denial record with a URL that has secrets
        let record = DenialRecord {
            tenant: "test".to_string(),
            purpose: Purpose::Script,
            actor: Some("user@test".to_string()),
            scheme: "https".to_string(),
            host: "api.example.com".to_string(),
            port: 443,
            resolved: vec!["8.8.8.8".parse().unwrap()],
            class: AddrClass::Public,
            rule: "allowed".to_string(),
            at: time::OffsetDateTime::now_utc(),
        };

        let summary = record.summary();
        // Summary should not contain query strings or secrets
        assert!(!summary.contains("?token="));
        assert!(!summary.contains("secret"));
        assert!(!summary.contains("password"));
        assert!(summary.contains("api.example.com"));
        assert!(summary.contains("443"));
    }

    #[test]
    fn test_address_classification_exhaustive() {
        // Test public addresses
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))),
            AddrClass::Public
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))),
            AddrClass::Public
        );

        // Test loopback
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
            AddrClass::Loopback
        );
        assert_eq!(
            classify(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            AddrClass::Loopback
        );

        // Test private
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            AddrClass::Private
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))),
            AddrClass::Private
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1))),
            AddrClass::Private
        );

        // Test cloud metadata
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))),
            AddrClass::CloudMetadata
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200))),
            AddrClass::CloudMetadata
        );

        // Test unspecified
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))),
            AddrClass::Unspecified
        );

        // Test unique-local (IPv6)
        let unique_local: IpAddr = "fc00::1".parse().unwrap();
        assert_eq!(classify(unique_local), AddrClass::UniqueLocal);

        // Test link-local
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))),
            AddrClass::LinkLocal
        );

        // Test multicast
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))),
            AddrClass::Multicast
        );
    }

    #[test]
    fn every_error_has_its_catalogue_code() {
        assert_eq!(EgressError::PolicyDenied.code(), "egress.denied_by_policy");
        assert_eq!(EgressError::RedirectDenied.code(), "egress.redirect_denied");
        assert_eq!(
            EgressError::RequestTooLarge { limit: 1 }.code(),
            "egress.request_too_large"
        );
        assert_eq!(
            EgressError::InvalidConfig(String::new()).code(),
            "egress.invalid_config"
        );
    }
}
