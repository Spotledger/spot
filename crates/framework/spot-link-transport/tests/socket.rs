//! The carriers over real sockets: mutual TLS on 127.0.0.1 with a CA generated in the test.
//!
//! Tenant A runs the only listener. Tenant B is "behind NAT": it never listens, and reaches A
//! only by dialling out, to push (`send = direct`) and to pull (`receive = pull`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(missing_docs)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::net::{client, code_of, config};
use common::*;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use spot_link_exchange::{EntryStatus, InboxStore, MessageState};
use spot_link_transport::{CarrierTable, LinkCarriers, codes};
use spot_sync_http::{ClientCertificate, HttpRequest, SocketTransport, Transport, tls};
use spot_tenant_link::Carrier;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

struct Fixture {
    ca_params: CertificateParams,
    ca: KeyPair,
    ca_certificate: rcgen::Certificate,
    server_chain: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
}

impl Fixture {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "spot-link-transport test CA");
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

    fn client(&self, common_name: &str) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let issuer = Issuer::from_params(&self.ca_params, &self.ca);
        issue(&issuer, common_name, Vec::new(), true)
    }

    /// A client certificate from this CA whose validity ended long ago.
    fn expired_client(
        &self,
        common_name: &str,
    ) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let issuer = Issuer::from_params(&self.ca_params, &self.ca);
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        let key = KeyPair::generate().unwrap();
        let certificate = params.signed_by(&key, &issuer).unwrap();
        (
            certificate.der().clone(),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
    }

    fn client_tls(
        &self,
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Arc<rustls::ClientConfig> {
        Arc::new(tls::client_config(self.ca_certificate.der().clone(), chain, key).unwrap())
    }
}

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

/// Serves A's router over mutual TLS and returns the address and a shutdown handle.
async fn serve(
    fixture: &Fixture,
    router: axum::Router,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    serve_with(fixture, fixture, router).await
}

/// Serves with `presenting`'s server certificate and requires client certificates that chain to
/// `trusting`'s CA.
async fn serve_with(
    presenting: &Fixture,
    trusting: &Fixture,
    router: axum::Router,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    let server_config = tls::server_config(
        presenting.server_chain.clone(),
        presenting.server_key.clone_key(),
        trusting.ca_certificate.der().clone(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
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

fn nat_a() -> LinkCarriers {
    LinkCarriers {
        send: Carrier::Pull,
        receive: Carrier::Direct,
    }
}

fn nat_b() -> LinkCarriers {
    LinkCarriers {
        send: Carrier::Direct,
        receive: Carrier::Pull,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pull_only_side_behind_nat_and_a_direct_side_exchange_over_mutual_tls() {
    let fixture = Fixture::new();
    let w = world();
    let (cert_der, key) = fixture.client("tenant-b");
    let cert_b = ClientCertificate::from_der(cert_der.to_vec());
    let carriers = Arc::new(CarrierTable::new());
    carriers.set(w.a.link, nat_a());
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, carriers);
    let (address, shutdown) = serve(&fixture, server_a.router()).await;

    let transport = Arc::new(SocketTransport::new(
        address,
        "127.0.0.1",
        fixture.client_tls(vec![cert_der], key),
    ));
    let client_b = client(&w.b, &w.a, transport, nat_b(), config());

    let to_b = w.a.enqueue("orders", "a to b");
    let to_a = w.b.enqueue("orders", "b to a");
    let pushed = client_b.push_step().await.unwrap();
    let pulled = client_b.pull_step().await.unwrap();
    assert_eq!((pushed.moved, pulled.moved), (1, 1));

    let applied = |node: &Node, id| {
        node.inbox
            .entry(&node.link, &id)
            .unwrap()
            .is_some_and(|entry| entry.status == EntryStatus::Applied)
    };
    assert!(applied(&w.a, to_a));
    assert!(applied(&w.b, to_b));
    assert_eq!(w.a.states(), vec![MessageState::Applied]);
    assert_eq!(w.b.states(), vec![MessageState::Applied]);
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_from_the_ca_that_is_not_pinned_is_refused_on_the_wire() {
    let fixture = Fixture::new();
    let w = world();
    let (pinned_der, _pinned_key) = fixture.client("tenant-b");
    let cert_b = ClientCertificate::from_der(pinned_der.to_vec());
    let carriers = Arc::new(CarrierTable::new());
    carriers.set(w.a.link, nat_a());
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, carriers);
    let (address, shutdown) = serve(&fixture, server_a.router()).await;

    // Chains to the CA, so TLS accepts it, but nothing pins it to a party.
    let (other_der, other_key) = fixture.client("intruder");
    let transport = SocketTransport::new(
        address,
        "127.0.0.1",
        fixture.client_tls(vec![other_der], other_key),
    );
    let response = transport
        .request(HttpRequest::get(format!("/link/v1/{}/mailbox", w.a.link)))
        .await
        .unwrap();
    assert_eq!(response.status, 403);
    assert_eq!(code_of(&response), codes::FORBIDDEN);
    assert!(
        w.a.audit.events().unwrap().is_empty(),
        "an outsider writes nothing to the trail"
    );
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_without_a_certificate_cannot_even_connect() {
    let fixture = Fixture::new();
    let w = world();
    let (der, _key) = fixture.client("tenant-b");
    let cert_b = ClientCertificate::from_der(der.to_vec());
    let carriers = Arc::new(CarrierTable::new());
    carriers.set(w.a.link, nat_a());
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, carriers);
    let (address, shutdown) = serve(&fixture, server_a.router()).await;

    // A TLS client that trusts the CA but presents no certificate.
    let mut roots = rustls::RootCertStore::empty();
    roots.add(fixture.ca_certificate.der().clone()).unwrap();
    let anonymous = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let transport = SocketTransport::new(address, "127.0.0.1", Arc::new(anonymous));
    let attempt = tokio::time::timeout(
        Duration::from_secs(10),
        transport.request(HttpRequest::get(format!("/link/v1/{}/mailbox", w.a.link))),
    )
    .await
    .unwrap();
    match attempt {
        Err(_) => {}
        Ok(response) => assert_ne!(response.status, 200, "an anonymous client got an answer"),
    }
    let _ = shutdown.send(());
}

/// What a client that cannot complete mutual TLS sees: the connection fails, or at the very least
/// nothing is answered with success.
async fn assert_no_service(transport: &SocketTransport, link: impl std::fmt::Display) {
    let attempt = tokio::time::timeout(
        Duration::from_secs(10),
        transport.request(HttpRequest::get(format!("/link/v1/{link}/mailbox"))),
    )
    .await
    .unwrap();
    match attempt {
        Err(_) => {}
        Ok(response) => assert_ne!(response.status, 200, "a refused client got an answer"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_client_certificate_from_an_unknown_ca_cannot_connect() {
    let fixture = Fixture::new();
    let foreign = Fixture::new();
    let w = world();
    // The foreign certificate is pinned to tenant B, but its CA is not the one the server trusts:
    // TLS refuses it before the directory is consulted.
    let (der, key) = foreign.client("tenant-b");
    let cert_b = ClientCertificate::from_der(der.to_vec());
    let carriers = Arc::new(CarrierTable::new());
    carriers.set(w.a.link, nat_a());
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, carriers);
    let (address, shutdown) = serve(&fixture, server_a.router()).await;

    let transport = SocketTransport::new(address, "127.0.0.1", fixture.client_tls(vec![der], key));
    assert_no_service(&transport, w.a.link).await;
    assert!(w.a.audit.events().unwrap().is_empty());
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_but_expired_client_certificate_cannot_connect() {
    let fixture = Fixture::new();
    let w = world();
    let (der, key) = fixture.expired_client("tenant-b");
    let cert_b = ClientCertificate::from_der(der.to_vec());
    let carriers = Arc::new(CarrierTable::new());
    carriers.set(w.a.link, nat_a());
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, carriers);
    let (address, shutdown) = serve(&fixture, server_a.router()).await;

    let transport = SocketTransport::new(address, "127.0.0.1", fixture.client_tls(vec![der], key));
    assert_no_service(&transport, w.a.link).await;
    assert!(w.a.audit.events().unwrap().is_empty());
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_whose_pin_was_revoked_is_refused_on_the_wire() {
    let fixture = Fixture::new();
    let w = world();
    let (der, key) = fixture.client("tenant-b");
    let cert_b = ClientCertificate::from_der(der.to_vec());
    let carriers = Arc::new(CarrierTable::new());
    carriers.set(w.a.link, nat_a());
    let (server_a, directory) = server_for(&w.a, &w.b.party, &cert_b, carriers);
    let (address, shutdown) = serve(&fixture, server_a.router()).await;

    let transport = SocketTransport::new(address, "127.0.0.1", fixture.client_tls(vec![der], key));
    let mailbox = || HttpRequest::get(format!("/link/v1/{}/mailbox", w.a.link));
    let before = transport.request(mailbox()).await.unwrap();
    assert_eq!(before.status, 200);

    directory.revoke(cert_b.fingerprint());
    let after = transport.request(mailbox()).await.unwrap();
    assert_eq!(
        (after.status, code_of(&after).as_str()),
        (403, codes::FORBIDDEN)
    );
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_certificate_from_an_unknown_ca_is_not_trusted_by_the_dialling_side() {
    let fixture = Fixture::new();
    let rogue = Fixture::new();
    let w = world();
    let (der, key) = fixture.client("tenant-b");
    let cert_b = ClientCertificate::from_der(der.to_vec());
    let carriers = Arc::new(CarrierTable::new());
    carriers.set(w.a.link, nat_a());
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, carriers);
    // A server that presents a certificate from a CA the dialler never pinned, but accepts the
    // dialler's own client certificate: the dialler must still refuse to talk to it.
    let (address, shutdown) = serve_with(&rogue, &fixture, server_a.router()).await;

    let transport = SocketTransport::new(address, "127.0.0.1", fixture.client_tls(vec![der], key));
    let attempt = tokio::time::timeout(
        Duration::from_secs(10),
        transport.request(HttpRequest::get(format!("/link/v1/{}/mailbox", w.a.link))),
    )
    .await
    .unwrap();
    assert!(
        attempt.is_err(),
        "the dialler talked to an untrusted server"
    );
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_certificate_for_another_name_is_not_accepted() {
    let fixture = Fixture::new();
    let w = world();
    let (der, key) = fixture.client("tenant-b");
    let cert_b = ClientCertificate::from_der(der.to_vec());
    let carriers = Arc::new(CarrierTable::new());
    carriers.set(w.a.link, nat_a());
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, carriers);
    let (address, shutdown) = serve(&fixture, server_a.router()).await;

    // The certificate is for localhost / 127.0.0.1; the link pins another host name.
    let transport =
        SocketTransport::new(address, "peer.example", fixture.client_tls(vec![der], key));
    let attempt = tokio::time::timeout(
        Duration::from_secs(10),
        transport.request(HttpRequest::get(format!("/link/v1/{}/mailbox", w.a.link))),
    )
    .await
    .unwrap();
    assert!(
        attempt.is_err(),
        "the dialler accepted a certificate for another name"
    );
    let _ = shutdown.send(());
}
