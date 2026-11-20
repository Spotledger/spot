//! WP-ADM-02.T6: the two trust modes of the TLS channel against a loopback TLS 1.3 gateway.
//! With a CA file the channel is pinned to it (the system roots are not consulted); with
//! the system roots (here handed in as DER, the form `sysroots` produces) the gateway's chain
//! must reach one of them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use spot_agent::channel::build_transport;
use spot_agent::channel::transport::{HttpRequest, TlsTransport, Transport};

/// A loopback TLS 1.3 gateway: answers every request with `{"data":{"commands":[]}}`.
fn tls_gateway(
    cert_der: rustls::pki_types::CertificateDer<'static>,
    key_der: Vec<u8>,
    connections: usize,
) -> u16 {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert_der],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key_der).into(),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(connections).flatten() {
            let conn = rustls::ServerConnection::new(config.clone()).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, stream);
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match tls.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let body = r#"{"data":{"commands":[]}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tls.write_all(response.as_bytes());
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
    });
    port
}

fn request() -> HttpRequest {
    HttpRequest {
        method: "POST",
        path: "/agent/v1/poll".to_owned(),
        headers: vec![],
        body: b"{}".to_vec(),
        timeout: Duration::from_secs(10),
    }
}

#[test]
fn a_ca_file_pins_the_channel_and_replaces_the_system_roots() {
    let real = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let other = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let port = tls_gateway(real.cert.der().clone(), real.signing_key.serialize_der(), 4);
    let url = format!("https://localhost:{port}");
    let dir = tempfile::tempdir().unwrap();

    // `--ca` with the gateway's certificate: the exchange works.
    let ca_ok = dir.path().join("ca.pem");
    std::fs::write(&ca_ok, real.cert.pem()).unwrap();
    let pinned = build_transport(&url, Some(&ca_ok)).unwrap();
    assert_eq!(pinned.send(&request()).unwrap().status, 200);

    // `--ca` with another certificate: refused, although the gateway's own certificate is
    // among the roots a system store could hold (the anchor replaces them).
    let ca_other = dir.path().join("other.pem");
    std::fs::write(&ca_other, other.cert.pem()).unwrap();
    let wrong = build_transport(&url, Some(&ca_other)).unwrap();
    assert_eq!(
        wrong.send(&request()).unwrap_err().code(),
        "agent.transport"
    );

    // A missing or empty CA file is an error at build time, never a silent fallback.
    assert!(build_transport(&url, Some(&dir.path().join("missing.pem"))).is_err());
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    assert!(build_transport(&url, Some(&empty)).is_err());
}

#[test]
fn system_roots_are_used_without_a_ca_file() {
    let real = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let other = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let port = tls_gateway(real.cert.der().clone(), real.signing_key.serialize_der(), 4);
    let url = format!("https://localhost:{port}");

    // The gateway's certificate is among the system roots: the exchange works.
    let trusted = TlsTransport::with_system_roots(
        &url,
        vec![other.cert.der().to_vec(), real.cert.der().to_vec()],
    )
    .unwrap();
    assert_eq!(trusted.send(&request()).unwrap().status, 200);

    // It is not among them: refused.
    let untrusted = TlsTransport::with_system_roots(&url, vec![other.cert.der().to_vec()]).unwrap();
    assert_eq!(
        untrusted.send(&request()).unwrap_err().code(),
        "agent.transport"
    );

    // An empty or unusable store is an error that points at `--ca`.
    assert!(TlsTransport::with_system_roots(&url, vec![]).is_err());
    let err = TlsTransport::with_system_roots(&url, vec![b"not a certificate".to_vec()])
        .err()
        .unwrap();
    assert!(err.to_string().contains("--ca"));
}
