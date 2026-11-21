//! The binding over a real socket: mutual TLS on `127.0.0.1`, with a self-signed CA generated in
//! the test. This proves node identity end to end — the hub extracts the peer certificate from
//! the TLS session and the directory maps it to a node — and that the same client works on a wire
//! as on the loopback.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(missing_docs)]

mod support;

use std::sync::Arc;

use support::*;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use spot_sync_http::error::IDENTITY_UNKNOWN;
use spot_sync_http::{
    ClientCertificate, EdgeClient, MemoryHub, NoJitter, NodeDirectory, SeededJitter,
    SocketTransport, StaticDirectory, SyncServer, TokioSleeper, tls,
};
use spot_sync_protocol::{NodeId, Offer, SourceId};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// A certificate authority and the hub's server certificate, both generated per test.
struct Fixture {
    ca_params: CertificateParams,
    ca: KeyPair,
    ca_certificate: rcgen::Certificate,
    server_chain: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
}

impl Fixture {
    /// A fresh CA and a server certificate for `127.0.0.1`.
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "spot-sync-http test CA");
        let ca = KeyPair::generate().unwrap();
        let ca_certificate = params.self_signed(&ca).unwrap();

        let issuer = Issuer::from_params(&params, &ca);
        let (server_chain, server_key) =
            issue(&issuer, "localhost", vec!["127.0.0.1".to_owned()], false);
        Self {
            ca_params: params,
            ca,
            ca_certificate,
            server_chain: vec![server_chain],
            server_key,
        }
    }

    /// Issues a client certificate for `common_name`.
    fn client(&self, common_name: &str) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let issuer = Issuer::from_params(&self.ca_params, &self.ca);
        issue(&issuer, common_name, Vec::new(), true)
    }

    /// A client TLS configuration trusting this CA and presenting `chain`/`key`.
    fn client_tls(
        &self,
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Arc<rustls::ClientConfig> {
        Arc::new(tls::client_config(self.ca_certificate.der().clone(), chain, key).unwrap())
    }
}

/// Issues a leaf certificate signed by the CA.
fn issue(
    issuer: &Issuer<'_, &KeyPair>,
    common_name: &str,
    sans: Vec<String>,
    client_auth: bool,
) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let mut params = CertificateParams::new(sans).unwrap();
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params.extended_key_usages = vec![if client_auth {
        ExtendedKeyUsagePurpose::ClientAuth
    } else {
        ExtendedKeyUsagePurpose::ServerAuth
    }];
    let key = KeyPair::generate().unwrap();
    let certificate = params.signed_by(&key, issuer).unwrap();
    (
        certificate.der().clone(),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
    )
}

/// Serves the hub over mutual TLS, returning the bound address and a shutdown handle.
async fn serve_hub(
    fixture: &Fixture,
    directory: Arc<dyn NodeDirectory>,
    stream: Arc<MemoryHub>,
    sink: Arc<TestReceiver>,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    let server_config = tls::server_config(
        fixture.server_chain.clone(),
        fixture.server_key.clone_key(),
        fixture.ca_certificate.der().clone(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let router = SyncServer::new(
        NodeId::new("head-office").unwrap(),
        directory,
        stream,
        sink,
        Arc::new(spot_core::SystemClock::new()),
    )
    .with_settings(settings())
    .with_identity(spot_sync_protocol::ProtocolRange::current(), 7)
    .router();
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = tls::serve(
            listener,
            TlsAcceptor::from(Arc::new(server_config)),
            router,
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await;
    });
    (address, shutdown)
}

/// A pull and a push converge over mutual TLS on a real socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_converges_over_mutual_tls() {
    let fixture = Fixture::new();
    let (client_cert, client_key) = fixture.client("edge-khi");
    let certificate = ClientCertificate::from_der(client_cert.to_vec());
    assert!(
        certificate
            .subject()
            .is_some_and(|subject| subject.contains("edge-khi")),
        "the subject the TLS layer presents is readable: {:?}",
        certificate.subject()
    );
    let node = NodeId::new("edge-khi#branch_01").unwrap();

    let pull_source = SourceId::new("head-office#hub").unwrap();
    let push_source = SourceId::new("edge-khi#branch_01").unwrap();
    let stream = Arc::new(MemoryHub::new());
    stream.append(&pull_source, records(&pull_source, 1, 60));
    let sink = Arc::new(spot_sync_http::SharedReceiver::new(receiver()));

    let mut directory = StaticDirectory::new();
    directory.enroll(
        certificate.fingerprint(),
        node.clone(),
        ["branch_01".to_owned()],
    );

    let (address, shutdown) = serve_hub(&fixture, Arc::new(directory), stream, sink).await;
    let client_tls = fixture.client_tls(vec![client_cert], client_key);
    let transport = Arc::new(SocketTransport::new(address, "127.0.0.1", client_tls));
    let mut client = EdgeClient::new(
        transport,
        identity(&node),
        client_config(64),
        Arc::new(spot_core::SystemClock::new()),
        Box::new(SeededJitter::new(1)),
        Arc::new(TokioSleeper),
        "hub:head-office",
    )
    .unwrap();

    let reply = client.hello().await.unwrap();
    assert_eq!(reply.hub.node, NodeId::new("head-office").unwrap());

    let mut receiver = receiver();
    let applied = client
        .pull_until_idle(&mut receiver, &pull_source, 20)
        .await
        .unwrap();
    assert_eq!(applied, 60);
    assert_eq!(receiver.sink().len(), 60);

    let offer = Offer::new(push_source.clone(), records(&push_source, 1, 40));
    let acked = client.push_until_idle(&offer, 20).await.unwrap();
    assert_eq!(acked, 40);

    let _ = shutdown.send(());
}

/// A certificate signed by the tenant CA but not enrolled in the directory is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unenrolled_certificate_is_refused_over_tls() {
    let fixture = Fixture::new();
    let (enrolled_cert, _enrolled_key) = fixture.client("edge-khi");
    let (unknown_cert, unknown_key) = fixture.client("edge-other");
    let node = NodeId::new("edge-khi#branch_01").unwrap();

    let mut directory = StaticDirectory::new();
    directory.enroll(
        ClientCertificate::from_der(enrolled_cert.to_vec()).fingerprint(),
        node.clone(),
        ["branch_01".to_owned()],
    );

    let (address, shutdown) = serve_hub(
        &fixture,
        Arc::new(directory),
        Arc::new(MemoryHub::new()),
        Arc::new(spot_sync_http::SharedReceiver::new(receiver())),
    )
    .await;
    let client_tls = fixture.client_tls(vec![unknown_cert], unknown_key);
    let transport = Arc::new(SocketTransport::new(address, "127.0.0.1", client_tls));
    let client = EdgeClient::new(
        transport,
        identity(&node),
        client_config(64),
        Arc::new(spot_core::SystemClock::new()),
        Box::new(NoJitter),
        Arc::new(TokioSleeper),
        "hub:head-office",
    )
    .unwrap();

    let error = client.hello().await.unwrap_err();
    assert_eq!(error.code, IDENTITY_UNKNOWN);
    let _ = shutdown.send(());
}
