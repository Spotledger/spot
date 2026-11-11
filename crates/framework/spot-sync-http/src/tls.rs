//! Mutual TLS for node identity: the configs, and the listener that puts the peer certificate
//! where the identity middleware expects it.
//!
//! ADR-062 section 3 requires TLS with node identity; ADR-078 section 11 says node identity may
//! use mutual TLS. This module builds the two rustls configurations (the hub requires a client
//! certificate and verifies it against the tenant CA; the edge presents its own and pins the
//! hub's CA) and serves a router over a TLS listener, extracting the peer certificate into a
//! [`ClientCertificate`](crate::identity::ClientCertificate) request extension.
//!
//! The directory still decides: a certificate that chains to the CA but is not enrolled is
//! refused by [`NodeDirectory`](crate::identity::NodeDirectory), not by rustls.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::http::Request;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::error::{Error, Result};
use crate::identity::ClientCertificate;

/// The crypto provider this crate uses. `ring` is chosen explicitly so the build never depends on
/// a process-default provider being installed (the reason spot-egress and spot-mail do the same).
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A client configuration that trusts `ca` and presents `chain`/`key`.
pub fn client_config(
    ca: CertificateDer<'static>,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(ca)
        .map_err(|error| Error::unavailable(format!("the hub CA did not load: {error}")))?;
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::unavailable(format!("TLS versions: {error}")))?
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)
        .map_err(|error| {
            Error::unavailable(format!("the client certificate did not load: {error}"))
        })?;
    Ok(config)
}

/// A server configuration that presents `chain`/`key` and requires a client certificate chained
/// to `client_ca`.
pub fn server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    client_ca: CertificateDer<'static>,
) -> Result<rustls::ServerConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(client_ca)
        .map_err(|error| Error::unavailable(format!("the client CA did not load: {error}")))?;
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|error| {
            Error::unavailable(format!("the client verifier did not build: {error}"))
        })?;
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::unavailable(format!("TLS versions: {error}")))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .map_err(|error| {
            Error::unavailable(format!("the server certificate did not load: {error}"))
        })?;
    Ok(config)
}

/// Serves `router` on `listener` over `acceptor` until `shutdown` completes.
///
/// Every connection's peer certificate is put into each request's extensions, so the identity
/// middleware resolves the node exactly as it would on a plain listener.
///
/// # Errors
///
/// [`Error::unavailable`] when the listener cannot accept or a connection cannot be served.
pub async fn serve<F>(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    router: Router,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut shutdown = Box::pin(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return Ok(()),
            accepted = listener.accept() => {
                let (tcp, _peer) = accepted
                    .map_err(|error| Error::unavailable(format!("accept failed: {error}")))?;
                let acceptor = acceptor.clone();
                let router = router.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_one(tcp, acceptor, router).await {
                        tracing::debug!(code = error.code, "sync connection ended");
                    }
                });
            }
        }
    }
}

/// Completes one TLS handshake and serves the connection.
async fn serve_one(
    tcp: tokio::net::TcpStream,
    acceptor: TlsAcceptor,
    router: Router,
) -> Result<()> {
    let tls = acceptor
        .accept(tcp)
        .await
        .map_err(|error| Error::unavailable(format!("TLS handshake failed: {error}")))?;
    let certificate = {
        let server_connection = tls.get_ref().1;
        server_connection
            .peer_certificates()
            .and_then(|chain| chain.first())
            .map(|der| ClientCertificate::from_der(der.to_vec()))
    };
    let service = TowerToHyperService::new(CertificateService {
        router,
        certificate,
    });
    let builder = auto::Builder::new(TokioExecutor::new());
    let connection = Box::pin(builder.serve_connection_with_upgrades(TokioIo::new(tls), service));
    connection
        .await
        .map_err(|error| Error::unavailable(format!("connection failed: {error}")))
}

/// One connection's service: injects the peer certificate, then hands the request to the router.
#[derive(Clone)]
struct CertificateService {
    router: Router,
    certificate: Option<ClientCertificate>,
}

impl tower::Service<Request<Incoming>> for CertificateService {
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = <Router as tower::Service<Request<Incoming>>>::Future;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut request: Request<Incoming>) -> Self::Future {
        if let Some(certificate) = &self.certificate {
            request.extensions_mut().insert(certificate.clone());
        }
        self.router.call(request)
    }
}
