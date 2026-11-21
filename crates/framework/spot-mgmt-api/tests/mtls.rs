//! Mutual TLS over a real socket with throwaway `rcgen` CAs (dev-dependency only).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use spot_core::{Clock, ManualClock};
use spot_mgmt_api::handlers::HandlerRegistry;
use spot_mgmt_api::lifecycle::MemoryCommandStore;
use spot_mgmt_api::ports::{MemoryAuditSink, NoApprovals, OperatorAllowList, OperatorEntry};
use spot_mgmt_api::stubs::stub_catalogue;
use spot_mgmt_api::tls::{HandshakeLimiter, serve};
use spot_mgmt_api::{Deps, router};
use spot_signed_command::MemoryNonceStore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;

struct Ca {
    params: CertificateParams,
    key: KeyPair,
    der: CertificateDer<'static>,
}

impl Ca {
    fn new(name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        let key = KeyPair::generate().unwrap();
        let der = params.self_signed(&key).unwrap().der().clone();
        Self { params, key, der }
    }

    fn issue(
        &self,
        common_name: &str,
        sans: Vec<String>,
        client: bool,
    ) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let issuer = Issuer::from_params(&self.params, &self.key);
        let mut params = CertificateParams::new(sans).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.extended_key_usages = vec![if client {
            ExtendedKeyUsagePurpose::ClientAuth
        } else {
            ExtendedKeyUsagePurpose::ServerAuth
        }];
        let key = KeyPair::generate().unwrap();
        let certificate = params.signed_by(&key, &issuer).unwrap();
        (
            certificate.der().clone(),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

async fn start(operator_ca: &Ca) -> (String, tokio::sync::oneshot::Sender<()>) {
    let (chain, key) = operator_ca.issue("localhost", vec!["127.0.0.1".to_owned()], false);
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
    ));
    let deps = Deps {
        catalogue: Arc::new(stub_catalogue().unwrap()),
        handlers: HandlerRegistry::new(),
        nonces: Arc::new(MemoryNonceStore::new()),
        administrator_keys: Vec::new(),
        operator_ca: vec![operator_ca.der.clone()],
        operators: OperatorAllowList::new(vec![OperatorEntry::new("CN=alice", "alice")]),
        approvals: Arc::new(NoApprovals),
        clock: Arc::clone(&clock),
        audit: Arc::new(MemoryAuditSink::new()),
        commands: Arc::new(MemoryCommandStore::new()),
        site_id: "site-1".to_owned(),
    };
    let config = deps.tls_config(vec![chain], key).unwrap();
    let app = router(deps);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (shutdown, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = serve(
            listener,
            Arc::new(config),
            app,
            clock,
            Arc::new(HandshakeLimiter::standard()),
            async move {
                let _ = rx.await;
            },
        )
        .await;
    });
    (address, shutdown)
}

/// GET `path`; `Some(status line + body)` only when an HTTP answer came back, `None` when the
/// handshake or the connection failed.
async fn get(
    address: &str,
    server_ca: &Ca,
    client: Option<(CertificateDer<'static>, PrivateKeyDer<'static>)>,
    path: &str,
) -> Option<String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(server_ca.der.clone()).unwrap();
    let builder = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots);
    let config = match client {
        Some((chain, key)) => builder.with_client_auth_cert(vec![chain], key).unwrap(),
        None => builder.with_no_client_auth(),
    };
    let tcp = TcpStream::connect(address).await.ok()?;
    let name = ServerName::try_from("127.0.0.1".to_owned()).unwrap();
    let mut tls = TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    tls.write_all(request.as_bytes()).await.ok()?;
    let mut buffer = Vec::new();
    // TLS 1.3 reports a rejected client certificate on the first read, not at connect.
    tls.read_to_end(&mut buffer).await.ok()?;
    let text = String::from_utf8_lossy(&buffer).into_owned();
    text.starts_with("HTTP/1.1").then_some(text)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_from_the_operator_ca_on_the_allow_list_is_served() {
    let ca = Ca::new("operator CA");
    let (address, shutdown) = start(&ca).await;
    let client = ca.issue("alice", Vec::new(), true);
    let answer = get(&address, &ca, Some(client), "/mgmt/v1/operations")
        .await
        .unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains("\"data\""));
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_client_certificate_is_refused_at_the_handshake() {
    let ca = Ca::new("operator CA");
    let (address, shutdown) = start(&ca).await;
    assert!(get(&address, &ca, None, "/mgmt/v1/health").await.is_none());
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_from_another_ca_is_refused_at_the_handshake() {
    let ca = Ca::new("operator CA");
    let other = Ca::new("some other CA");
    let (address, shutdown) = start(&ca).await;
    let foreign = other.issue("alice", Vec::new(), true);
    assert!(
        get(&address, &ca, Some(foreign), "/mgmt/v1/operations")
            .await
            .is_none()
    );
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_certificate_off_the_allow_list_is_forbidden() {
    let ca = Ca::new("operator CA");
    let (address, shutdown) = start(&ca).await;
    let client = ca.issue("mallory", Vec::new(), true);
    let answer = get(&address, &ca, Some(client), "/mgmt/v1/operations")
        .await
        .unwrap();
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    assert!(answer.contains("mgmt.forbidden"));
    let _ = shutdown.send(());
}

#[test]
fn the_handshake_limiter_counts_per_address_and_window() {
    let limiter = HandshakeLimiter::new(10, 3);
    let a: std::net::IpAddr = "10.0.0.1".parse().unwrap();
    let b: std::net::IpAddr = "10.0.0.2".parse().unwrap();
    assert!((0..3).all(|_| limiter.allow(a, 100)));
    assert!(!limiter.allow(a, 101));
    assert!(limiter.allow(b, 101));
    assert!(limiter.allow(a, 111));
}

#[test]
fn an_empty_operator_ca_bundle_refuses_to_build() {
    let ca = Ca::new("operator CA");
    let (chain, key) = ca.issue("localhost", vec!["127.0.0.1".to_owned()], false);
    assert!(spot_mgmt_api::tls_config(vec![chain], key, &[]).is_err());
}
