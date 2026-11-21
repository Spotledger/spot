//! Mutual TLS: the server configuration (TLS 1.3 only, a client certificate from the operator CA
//! is required at the handshake) and the listener that hands the verified peer certificate to the
//! router. Unauthenticated handshakes are rate limited per source address.

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::Request;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use spot_core::Clock;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tower::Service;

/// A handshake that takes longer than this is dropped.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The client certificate the TLS layer verified, as the router sees it.
///
/// Only the TLS layer of this crate (or a test harness standing in for it) creates one: the
/// router trusts the extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerCertificate {
    subject: Option<String>,
    fingerprint: String,
}

impl PeerCertificate {
    /// From a certificate's DER bytes: the subject is parsed when the bytes are X.509.
    #[must_use]
    pub fn from_der(der: &[u8]) -> Self {
        let digest = Sha256::digest(der);
        let mut fingerprint = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(fingerprint, "{byte:02x}");
        }
        Self {
            subject: subject_of(der),
            fingerprint,
        }
    }

    /// A certificate identity from parts (in-process tests).
    #[must_use]
    pub fn new(subject: &str, fingerprint: &str) -> Self {
        Self {
            subject: Some(subject.to_owned()),
            fingerprint: fingerprint.to_owned(),
        }
    }

    /// The subject (RFC 2253 text), when it parsed.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    /// The SHA-256 fingerprint, lower-case hex.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

fn subject_of(der: &[u8]) -> Option<String> {
    use x509_parser::prelude::FromDer as _;
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(der).ok()?;
    Some(certificate.subject().to_string())
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Why the TLS configuration could not be built.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TlsConfigError {
    /// The operator CA bundle is empty or a certificate in it does not load.
    #[error("the operator CA bundle did not load")]
    OperatorCa,
    /// The server certificate chain or key does not load or does not match.
    #[error("the server certificate or key did not load")]
    ServerCertificate,
}

/// The server configuration: TLS 1.3 only, a client certificate that chains to `operator_ca` is
/// required (no certificate or one from another CA fails the handshake).
///
/// # Errors
/// [`TlsConfigError`] for an unusable CA bundle, chain or key.
pub fn tls_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    operator_ca: &[CertificateDer<'static>],
) -> Result<rustls::ServerConfig, TlsConfigError> {
    if operator_ca.is_empty() {
        return Err(TlsConfigError::OperatorCa);
    }
    let mut roots = rustls::RootCertStore::empty();
    for certificate in operator_ca {
        roots
            .add(certificate.clone())
            .map_err(|_| TlsConfigError::OperatorCa)?;
    }
    let verifier =
        rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
            .build()
            .map_err(|_| TlsConfigError::OperatorCa)?;
    rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| TlsConfigError::ServerCertificate)?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .map_err(|_| TlsConfigError::ServerCertificate)
}

/// Per-source-address limit on connection attempts, before any handshake work.
#[derive(Debug)]
pub struct HandshakeLimiter {
    window_secs: u64,
    max_per_window: u32,
    seen: Mutex<HashMap<IpAddr, (u64, u32)>>,
}

impl HandshakeLimiter {
    /// At most `max_per_window` attempts per address per `window_secs`.
    #[must_use]
    pub fn new(window_secs: u64, max_per_window: u32) -> Self {
        Self {
            window_secs: window_secs.max(1),
            max_per_window,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// 60 attempts per 10 seconds per address.
    #[must_use]
    pub fn standard() -> Self {
        Self::new(10, 60)
    }

    /// Counts an attempt at monotonic second `now`; false when the address is over its limit.
    #[must_use]
    pub fn allow(&self, address: IpAddr, now: u64) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return false;
        };
        if seen.len() > 10_000 {
            let window = self.window_secs;
            seen.retain(|_, (start, _)| now.saturating_sub(*start) < window);
        }
        let entry = seen.entry(address).or_insert((now, 0));
        if now.saturating_sub(entry.0) >= self.window_secs {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1 <= self.max_per_window
    }
}

/// The server configuration a listener hands to each new connection, replaceable at run time:
/// a reload (new certificate, rotated operator CA) applies to connections accepted afterwards,
/// and connections already open keep the configuration they were accepted with.
pub struct ReloadableTls {
    current: std::sync::RwLock<Arc<rustls::ServerConfig>>,
}

impl ReloadableTls {
    /// Wraps the initial configuration.
    #[must_use]
    pub fn new(config: rustls::ServerConfig) -> Arc<Self> {
        Arc::new(Self {
            current: std::sync::RwLock::new(Arc::new(config)),
        })
    }

    /// Replaces the configuration for connections accepted from now on.
    pub fn replace(&self, config: rustls::ServerConfig) {
        if let Ok(mut current) = self.current.write() {
            *current = Arc::new(config);
        }
    }

    /// The configuration new connections get.
    #[must_use]
    pub fn current(&self) -> Option<Arc<rustls::ServerConfig>> {
        self.current.read().ok().map(|guard| Arc::clone(&guard))
    }
}

/// Serves `router` on `listener` over TLS until `shutdown` completes.
///
/// # Errors
/// The accept error that ended the loop.
pub async fn serve<F>(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    router: Router,
    clock: Arc<dyn Clock>,
    limiter: Arc<HandshakeLimiter>,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let tls = Arc::new(ReloadableTls {
        current: std::sync::RwLock::new(config),
    });
    serve_reloadable(listener, tls, router, clock, limiter, shutdown).await
}

/// [`serve`] with a configuration that can be replaced while the listener runs.
///
/// # Errors
/// The accept error that ended the loop.
pub async fn serve_reloadable<F>(
    listener: TcpListener,
    tls: Arc<ReloadableTls>,
    router: Router,
    clock: Arc<dyn Clock>,
    limiter: Arc<HandshakeLimiter>,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut shutdown = Box::pin(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return Ok(()),
            accepted = listener.accept() => {
                let (tcp, peer) = accepted?;
                if !limiter.allow(peer.ip(), clock.monotonic().as_secs()) {
                    drop(tcp);
                    continue;
                }
                let Some(config) = tls.current() else {
                    drop(tcp);
                    continue;
                };
                let acceptor = TlsAcceptor::from(config);
                let router = router.clone();
                tokio::spawn(async move {
                    if serve_one(tcp, acceptor, router).await.is_err() {
                        tracing::debug!("management connection ended");
                    }
                });
            }
        }
    }
}

async fn serve_one(tcp: TcpStream, acceptor: TlsAcceptor, router: Router) -> Result<(), ()> {
    let tls = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    let certificate = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .map(|der| PeerCertificate::from_der(der.as_ref()));
    let service = TowerToHyperService::new(CertificateService {
        router,
        certificate,
    });
    let builder = auto::Builder::new(TokioExecutor::new());
    builder
        .serve_connection_with_upgrades(TokioIo::new(tls), service)
        .await
        .map_err(|_| ())
}

#[derive(Clone)]
struct CertificateService {
    router: Router,
    certificate: Option<PeerCertificate>,
}

impl Service<Request<Incoming>> for CertificateService {
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = <Router as Service<Request<Incoming>>>::Future;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut request: Request<Incoming>) -> Self::Future {
        if let Some(certificate) = &self.certificate {
            request.extensions_mut().insert(certificate.clone());
        }
        self.router.call(request)
    }
}
