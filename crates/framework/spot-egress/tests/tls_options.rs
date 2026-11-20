//! TLS options of the `HttpSender` against in-process rustls servers on loopback: private
//! trust anchors, mutual TLS, a TLS 1.3 floor, server-name pinning, redirects across origins,
//! build-time refusals and `Debug` secrecy.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use http::HeaderMap;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use spot_core::SystemClock;
use spot_egress::{
    CertPem, ClientIdentity, DenialSink, EgressConnector, EgressError, EgressPolicy, Guard,
    HttpSender, InMemorySink, Purpose, TestResolver, TlsOptions, TlsSummary, TlsVersion, TrustMode,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use url::Url;

// ---------------------------------------------------------------------------------------------
// Certificates

struct Ca {
    params: CertificateParams,
    key: KeyPair,
    cert: rcgen::Certificate,
}

struct Leaf {
    cert_pem: String,
    key_pem: String,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl Ca {
    fn new(common_name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        Self { params, key, cert }
    }

    fn pem(&self) -> CertPem {
        CertPem::new(self.cert.pem())
    }

    fn issue(&self, names: &[&str], client_auth: bool) -> Leaf {
        let mut params =
            CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())
                .unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, names.first().copied().unwrap_or("leaf"));
        if client_auth {
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        }
        let key = KeyPair::generate().unwrap();
        let issuer = Issuer::from_params(&self.params, &self.key);
        let cert = params.signed_by(&key, &issuer).unwrap();
        Leaf {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            chain: vec![cert.der().clone()],
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        }
    }
}

impl Leaf {
    fn identity(&self) -> ClientIdentity {
        ClientIdentity::from_pem(self.cert_pem.clone(), self.key_pem.clone())
    }
}

// ---------------------------------------------------------------------------------------------
// Servers

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Clone, Copy)]
enum ClientAuth {
    None,
    Optional,
    Required,
}

fn server_config(
    leaf: &Leaf,
    client_ca: Option<&Ca>,
    auth: ClientAuth,
    tls12_only: bool,
) -> rustls::ServerConfig {
    let versions: &[&'static rustls::SupportedProtocolVersion] = if tls12_only {
        &[&rustls::version::TLS12]
    } else {
        &[&rustls::version::TLS12, &rustls::version::TLS13]
    };
    let builder = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .unwrap();
    let builder = match (auth, client_ca) {
        (ClientAuth::None, _) | (_, None) => builder.with_no_client_auth(),
        (mode, Some(ca)) => {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(ca.cert.der().clone()).unwrap();
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                provider(),
            );
            let verifier = match mode {
                ClientAuth::Optional => verifier.allow_unauthenticated().build().unwrap(),
                _ => verifier.build().unwrap(),
            };
            builder.with_client_cert_verifier(verifier)
        }
    };
    builder
        .with_single_cert(leaf.chain.clone(), leaf.key.clone_key())
        .unwrap()
}

#[derive(Clone, Copy, Debug)]
struct Seen {
    client_cert: bool,
    tls13: bool,
}

struct TlsServer {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    failed_handshakes: Arc<AtomicUsize>,
}

fn http_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn http_redirect(to: &str) -> String {
    format!(
        "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

async fn tls_serve(config: rustls::ServerConfig, response: String) -> TlsServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let failed_handshakes = Arc::new(AtomicUsize::new(0));
    let (seen2, failed2) = (seen.clone(), failed_handshakes.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let (seen, failed, response) = (seen2.clone(), failed2.clone(), response.clone());
            tokio::spawn(async move {
                match acceptor.accept(tcp).await {
                    Err(_) => {
                        failed.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(mut tls) => {
                        let (_, conn) = tls.get_ref();
                        seen.lock().unwrap().push(Seen {
                            client_cert: conn.peer_certificates().is_some(),
                            tls13: conn.protocol_version()
                                == Some(rustls::ProtocolVersion::TLSv1_3),
                        });
                        let mut buf = vec![0u8; 8192];
                        let _ = tls.read(&mut buf).await;
                        let _ = tls.write_all(response.as_bytes()).await;
                        let _ = tls.shutdown().await;
                    }
                }
            });
        }
    });
    TlsServer {
        port,
        seen,
        failed_handshakes,
    }
}

impl TlsServer {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------------------------
// The client side: a guard that allows exactly the test hosts and ports on loopback

struct Rig {
    connector: EgressConnector,
    sink: Arc<InMemorySink>,
}

fn rig(hosts: &[(&str, u16)]) -> Rig {
    let mut toml = String::from("[tenants.t]\n");
    let resolver = Arc::new(TestResolver::new());
    for (host, port) in hosts {
        toml.push_str(&format!(
            "[[tenants.t.allow]]\nscheme = \"https\"\nhost = \"{host}\"\nports = [{port}]\nallow_private = [\"127.0.0.1\"]\n"
        ));
        resolver.add_response(host, vec![vec!["127.0.0.1".parse().unwrap()]; 16]);
    }
    let sink = Arc::new(InMemorySink::new());
    let guard = Arc::new(Guard::new(
        EgressPolicy::from_toml(&toml).unwrap(),
        resolver,
        sink.clone(),
        Arc::new(SystemClock::new()),
    ));
    Rig {
        connector: EgressConnector::new(guard, "t", Purpose::Connector),
        sink,
    }
}

fn sender(rig: &Rig, options: TlsOptions) -> HttpSender {
    HttpSender::with_tls(rig.connector.clone(), Duration::from_secs(5), options)
}

async fn get(rig: &Rig, sender: &HttpSender, host: &str, port: u16) -> Result<String, EgressError> {
    let url: Url = format!("https://{host}:{port}/").parse().unwrap();
    let response = rig
        .connector
        .follow_redirects(sender, &url, HeaderMap::new(), 5)
        .await?;
    assert_eq!(response.status, http::StatusCode::OK);
    Ok(String::from_utf8(response.body).unwrap())
}

fn only(ca: &Ca) -> TrustMode {
    TrustMode::OnlyPem(vec![ca.pem()])
}

fn opts(trust: TrustMode, identity: Option<ClientIdentity>) -> TlsOptions {
    TlsOptions::new(trust, identity, TlsVersion::Tls12, None).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Trust

#[tokio::test]
async fn private_ca_server_fails_under_public_roots_and_works_with_pem_anchors() {
    let ca = Ca::new("operator CA");
    let leaf = ca.issue(&["svc.test"], false);
    let server = tls_serve(
        server_config(&leaf, None, ClientAuth::None, false),
        http_ok("private"),
    )
    .await;
    let rig = rig(&[("svc.test", server.port)]);

    let public = sender(&rig, TlsOptions::public_roots().unwrap());
    assert!(get(&rig, &public, "svc.test", server.port).await.is_err());

    let plus = sender(&rig, opts(TrustMode::PublicPlusPem(vec![ca.pem()]), None));
    assert_eq!(
        get(&rig, &plus, "svc.test", server.port).await.unwrap(),
        "private"
    );

    let pinned = sender(&rig, opts(only(&ca), None));
    assert_eq!(
        get(&rig, &pinned, "svc.test", server.port).await.unwrap(),
        "private"
    );
}

#[tokio::test]
async fn only_pem_refuses_a_server_signed_by_a_different_ca() {
    let operator = Ca::new("operator CA");
    let public_looking = Ca::new("Trusted Public Root CA");
    let leaf = public_looking.issue(&["svc.test"], false);
    let server = tls_serve(
        server_config(&leaf, None, ClientAuth::None, false),
        http_ok("x"),
    )
    .await;
    let rig = rig(&[("svc.test", server.port)]);
    let pinned = sender(&rig, opts(only(&operator), None));
    assert!(get(&rig, &pinned, "svc.test", server.port).await.is_err());
    // Trusting the other CA too would make it work: the refusal is about the anchor.
    let both = sender(
        &rig,
        opts(
            TrustMode::OnlyPem(vec![operator.pem(), public_looking.pem()]),
            None,
        ),
    );
    assert_eq!(
        get(&rig, &both, "svc.test", server.port).await.unwrap(),
        "x"
    );
}

// ---------------------------------------------------------------------------------------------
// Mutual TLS

#[tokio::test]
async fn server_requiring_a_client_certificate_refuses_without_identity_and_accepts_with_one() {
    let ca = Ca::new("operator CA");
    let leaf = ca.issue(&["svc.test"], false);
    let client = ca.issue(&["spot-host"], true);
    let server = tls_serve(
        server_config(&leaf, Some(&ca), ClientAuth::Required, false),
        http_ok("mtls"),
    )
    .await;
    let rig = rig(&[("svc.test", server.port)]);

    let anonymous = sender(&rig, opts(only(&ca), None));
    assert!(
        get(&rig, &anonymous, "svc.test", server.port)
            .await
            .is_err()
    );

    let with_identity = sender(&rig, opts(only(&ca), Some(client.identity())));
    assert_eq!(
        get(&rig, &with_identity, "svc.test", server.port)
            .await
            .unwrap(),
        "mtls"
    );
    let seen = server.seen();
    assert_eq!(
        seen.len(),
        1,
        "only the identified request completed a handshake"
    );
    assert!(seen[0].client_cert);
}

// ---------------------------------------------------------------------------------------------
// Server-name pinning

#[tokio::test]
async fn wrong_name_certificate_fails_unless_the_override_names_it() {
    let ca = Ca::new("operator CA");
    // The peer is reached as `svc.test` (by address) but its certificate names `kms.internal`.
    let leaf = ca.issue(&["kms.internal"], false);
    let server = tls_serve(
        server_config(&leaf, None, ClientAuth::None, false),
        http_ok("kms"),
    )
    .await;
    let rig = rig(&[("svc.test", server.port)]);

    let plain = sender(&rig, opts(only(&ca), None));
    assert!(get(&rig, &plain, "svc.test", server.port).await.is_err());

    let overridden = sender(
        &rig,
        TlsOptions::new(
            only(&ca),
            None,
            TlsVersion::Tls12,
            Some("KMS.internal".into()),
        )
        .unwrap(),
    );
    assert_eq!(overridden_get(&rig, &overridden, server.port).await, "kms");
}

async fn overridden_get(rig: &Rig, sender: &HttpSender, port: u16) -> String {
    get(rig, sender, "svc.test", port).await.unwrap()
}

#[tokio::test]
async fn a_wrong_override_is_refused_and_recorded_in_the_denial_rule() {
    let ca = Ca::new("operator CA");
    let leaf = ca.issue(&["kms.internal"], false);
    let server = tls_serve(
        server_config(&leaf, None, ClientAuth::None, false),
        http_ok("kms"),
    )
    .await;
    let rig = rig(&[("svc.test", server.port)]);
    let wrong = TlsOptions::new(
        only(&ca),
        None,
        TlsVersion::Tls12,
        Some("someone-else.internal".into()),
    )
    .unwrap();
    let sender = sender(&rig, wrong).with_denial_context(Purpose::Connector, Some("op".into()));
    assert!(get(&rig, &sender, "svc.test", server.port).await.is_err());
    let records = rig.sink.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].tenant, "t");
    assert_eq!(records[0].host, "svc.test");
    assert_eq!(records[0].scheme, "https");
    assert_eq!(records[0].actor.as_deref(), Some("op"));
    assert!(
        records[0].rule.contains("tls_refused")
            && records[0]
                .rule
                .contains("server_name_override=someone-else.internal"),
        "{}",
        records[0].rule
    );
}

#[tokio::test]
async fn without_a_denial_context_a_refusal_writes_no_record() {
    let ca = Ca::new("operator CA");
    let other = Ca::new("other CA");
    let leaf = other.issue(&["svc.test"], false);
    let server = tls_serve(
        server_config(&leaf, None, ClientAuth::None, false),
        http_ok("x"),
    )
    .await;
    let rig = rig(&[("svc.test", server.port)]);
    let s = sender(&rig, opts(only(&ca), None));
    assert!(get(&rig, &s, "svc.test", server.port).await.is_err());
    assert!(rig.sink.records().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Protocol floor

#[tokio::test]
async fn tls13_floor_refuses_a_tls12_only_server() {
    let ca = Ca::new("operator CA");
    let leaf = ca.issue(&["svc.test"], false);
    let server = tls_serve(
        server_config(&leaf, None, ClientAuth::None, true),
        http_ok("old"),
    )
    .await;
    let rig = rig(&[("svc.test", server.port)]);

    let default = sender(&rig, opts(only(&ca), None));
    assert_eq!(
        get(&rig, &default, "svc.test", server.port).await.unwrap(),
        "old"
    );
    assert!(!server.seen()[0].tls13);

    let strict = sender(
        &rig,
        TlsOptions::new(only(&ca), None, TlsVersion::Tls13, None).unwrap(),
    );
    assert!(get(&rig, &strict, "svc.test", server.port).await.is_err());
    for _ in 0..50 {
        if server.failed_handshakes.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(server.failed_handshakes.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn tls13_floor_connects_to_a_tls13_server() {
    let ca = Ca::new("operator CA");
    let leaf = ca.issue(&["svc.test"], false);
    let server = tls_serve(
        server_config(&leaf, None, ClientAuth::None, false),
        http_ok("new"),
    )
    .await;
    let rig = rig(&[("svc.test", server.port)]);
    let strict = sender(
        &rig,
        TlsOptions::new(only(&ca), None, TlsVersion::Tls13, None).unwrap(),
    );
    assert_eq!(
        get(&rig, &strict, "svc.test", server.port).await.unwrap(),
        "new"
    );
    assert!(server.seen()[0].tls13);
}

// ---------------------------------------------------------------------------------------------
// Redirects

#[tokio::test]
async fn a_redirect_to_another_origin_never_offers_the_client_certificate() {
    for explicit_binding in [true, false] {
        let ca = Ca::new("operator CA");
        let leaf_a = ca.issue(&["svc.test"], false);
        let leaf_b = ca.issue(&["other.test"], false);
        let client = ca.issue(&["spot-host"], true);

        // B accepts but does not require a client certificate, and records whether one came.
        let b = tls_serve(
            server_config(&leaf_b, Some(&ca), ClientAuth::Optional, false),
            http_ok("second"),
        )
        .await;
        let a = tls_serve(
            server_config(&leaf_a, Some(&ca), ClientAuth::Required, false),
            http_redirect(&format!("https://other.test:{}/", b.port)),
        )
        .await;
        let rig = rig(&[("svc.test", a.port), ("other.test", b.port)]);
        let mut identity = client.identity();
        if explicit_binding {
            identity = identity.bound_to("svc.test", a.port);
        }
        let sender = sender(&rig, opts(only(&ca), Some(identity)));

        let body = get(&rig, &sender, "svc.test", a.port).await.unwrap();
        assert_eq!(body, "second", "explicit_binding={explicit_binding}");

        let seen_a = a.seen();
        assert_eq!(seen_a.len(), 1);
        assert!(seen_a[0].client_cert, "the first origin got the identity");
        let seen_b = b.seen();
        assert_eq!(seen_b.len(), 1);
        assert!(
            !seen_b[0].client_cert,
            "the second origin must not be offered the client certificate"
        );
    }
}

#[tokio::test]
async fn an_explicitly_bound_identity_is_not_sent_to_the_first_other_origin() {
    // The sender's first request goes to a different origin than the identity is bound to.
    let ca = Ca::new("operator CA");
    let leaf = ca.issue(&["other.test"], false);
    let client = ca.issue(&["spot-host"], true);
    let server = tls_serve(
        server_config(&leaf, Some(&ca), ClientAuth::Optional, false),
        http_ok("x"),
    )
    .await;
    let rig = rig(&[("other.test", server.port)]);
    let identity = client.identity().bound_to("svc.test", 1);
    let sender = sender(&rig, opts(only(&ca), Some(identity)));
    assert_eq!(
        get(&rig, &sender, "other.test", server.port).await.unwrap(),
        "x"
    );
    assert!(!server.seen()[0].client_cert);
}

#[tokio::test]
async fn the_name_override_applies_to_the_peer_origin_only() {
    let ca = Ca::new("operator CA");
    let leaf_a = ca.issue(&["kms.internal"], false);
    let leaf_b = ca.issue(&["other.test"], false);
    let b = tls_serve(
        server_config(&leaf_b, None, ClientAuth::None, false),
        http_ok("second"),
    )
    .await;
    let a = tls_serve(
        server_config(&leaf_a, None, ClientAuth::None, false),
        http_redirect(&format!("https://other.test:{}/", b.port)),
    )
    .await;
    let rig = rig(&[("svc.test", a.port), ("other.test", b.port)]);
    let options = TlsOptions::new(
        only(&ca),
        None,
        TlsVersion::Tls12,
        Some("kms.internal".into()),
    )
    .unwrap();
    let sender = sender(&rig, options);
    // A is verified against the override, B (another origin) against its own host name.
    assert_eq!(
        get(&rig, &sender, "svc.test", a.port).await.unwrap(),
        "second"
    );
}

// ---------------------------------------------------------------------------------------------
// Build-time refusals

fn assert_invalid(result: Result<TlsOptions, EgressError>) {
    let err = result.expect_err("must be refused");
    assert!(matches!(err, EgressError::InvalidTlsOptions(_)), "{err:?}");
    assert_eq!(err.code(), "egress.invalid_tls_options");
    let text = format!("{err} {err:?}");
    assert!(
        !text.contains("BEGIN") && !text.contains("PRIVATE"),
        "{text}"
    );
}

#[test]
fn invalid_material_is_refused_at_build() {
    let ca = Ca::new("operator CA");
    let one = ca.issue(&["a.test"], true);
    let two = ca.issue(&["b.test"], true);

    // PEM that is not PEM, PEM with no certificate, an empty anchor list.
    assert_invalid(TlsOptions::new(
        TrustMode::OnlyPem(vec![CertPem::new("not a pem at all")]),
        None,
        TlsVersion::Tls12,
        None,
    ));
    assert_invalid(TlsOptions::new(
        TrustMode::OnlyPem(vec![CertPem::new(
            "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n",
        )]),
        None,
        TlsVersion::Tls12,
        None,
    ));
    assert_invalid(TlsOptions::new(
        TrustMode::OnlyPem(vec![]),
        None,
        TlsVersion::Tls12,
        None,
    ));

    // An empty chain.
    assert_invalid(TlsOptions::new(
        only(&ca),
        Some(ClientIdentity::from_pem("", one.key_pem.clone())),
        TlsVersion::Tls12,
        None,
    ));
    // A key that does not match the chain.
    assert_invalid(TlsOptions::new(
        only(&ca),
        Some(ClientIdentity::from_pem(
            one.cert_pem.clone(),
            two.key_pem.clone(),
        )),
        TlsVersion::Tls12,
        None,
    ));
    // A key that is not PEM.
    assert_invalid(TlsOptions::new(
        only(&ca),
        Some(ClientIdentity::from_pem(one.cert_pem.clone(), "garbage")),
        TlsVersion::Tls12,
        None,
    ));
    // Overrides that are not one exact DNS name.
    for bad in ["*.example.com", "", "10.0.0.1", "bad name"] {
        assert_invalid(TlsOptions::new(
            only(&ca),
            None,
            TlsVersion::Tls12,
            Some(bad.to_owned()),
        ));
    }
    // And the same inputs, valid, build.
    assert!(
        TlsOptions::new(
            only(&ca),
            Some(one.identity()),
            TlsVersion::Tls13,
            Some("peer.example.com".into())
        )
        .is_ok()
    );
}

// ---------------------------------------------------------------------------------------------
// Defaults and secrecy

#[test]
fn the_default_sender_keeps_todays_tls() {
    let rig = rig(&[]);
    let sender = HttpSender::new(rig.connector.clone(), Duration::from_secs(1));
    assert_eq!(
        sender.tls_summary(),
        Some(TlsSummary {
            trust: "public_roots",
            client_identity: false,
            min_version: TlsVersion::Tls12,
            server_name_override: false,
        })
    );
    let explicit = TlsOptions::public_roots().unwrap();
    assert_eq!(Some(explicit.summary()), sender.tls_summary());
    assert_eq!(explicit.trust(), &TrustMode::PublicRoots);
    assert!(explicit.client_identity().is_none());
    assert_eq!(TlsVersion::default(), TlsVersion::Tls12);
}

#[test]
fn debug_output_never_shows_pem_or_key_bytes() {
    let ca = Ca::new("canary CA");
    let client = ca.issue(&["canary.test"], true);
    let key_body: String = client
        .key_pem
        .lines()
        .nth(1)
        .unwrap()
        .chars()
        .take(24)
        .collect();
    let cert_body: String = client
        .cert_pem
        .lines()
        .nth(1)
        .unwrap()
        .chars()
        .take(24)
        .collect();
    let options = TlsOptions::new(
        TrustMode::PublicPlusPem(vec![ca.pem()]),
        Some(client.identity().bound_to("canary.test", 443)),
        TlsVersion::Tls13,
        Some("canary.test".into()),
    )
    .unwrap();

    let renderings = [
        format!("{options:?}"),
        format!("{options:#?}"),
        format!("{:?}", options.client_identity().unwrap()),
        format!("{:#?}", options.client_identity().unwrap()),
        format!("{:?}", options.trust()),
        format!("{:?}", ca.pem()),
        format!("{:?}", client.identity()),
        format!("{:?}", options.summary()),
    ];
    for text in renderings {
        for needle in [
            key_body.as_str(),
            cert_body.as_str(),
            "BEGIN",
            "PRIVATE KEY",
            "END CERTIFICATE",
        ] {
            assert!(
                !text.contains(needle),
                "Debug output leaks {needle:?}: {text}"
            );
        }
    }
}
