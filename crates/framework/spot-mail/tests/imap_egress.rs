//! The IMAP connection dials only pinned addresses (the egress guard's answer) and never
//! resolves the host: the host name is the TLS server name and nothing else.

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use spot_mail::error::{MailError, PermanentFailure, TransientFailure};
use spot_mail::imap::{FetchLimits, ImapConfig, ImapSecurity, fetch_new, fetch_new_pinned};
use spot_mail::message::Secret;
use spot_mail::smtp::Credentials;
use support::imap_server::{ImapFakeServer, ImapScript, spawn_tls};
use support::{Must, MustErr};

/// A config naming `host` only for TLS, trusting the fake server's self-signed certificate
/// (valid for `127.0.0.1` and `mail.spot.test`).
fn config(server: &ImapFakeServer, host: &str) -> ImapConfig {
    ImapConfig {
        host: host.to_owned(),
        port: server.port(),
        security: ImapSecurity::Tls,
        credentials: Credentials {
            user: support::FAKE_USER.to_owned(),
            secret: Secret::new(support::FAKE_PASSWORD),
        },
        mailbox: "INBOX".to_owned(),
        timeout: Duration::from_secs(5),
        root_certificate_pem: Some(server.host_certificate_pem().must().as_bytes().to_vec()),
    }
}

fn pinned(server: &ImapFakeServer) -> Vec<SocketAddr> {
    vec![SocketAddr::from(([127, 0, 0, 1], server.port()))]
}

fn is_pinned_refusal(error: &MailError) -> bool {
    matches!(error, MailError::Permanent(PermanentFailure::Client(reason))
        if reason.contains("pinned addresses are required"))
}

#[tokio::test]
async fn a_pinned_connection_never_resolves_the_host() {
    let server = spawn_tls().await;
    server.set_script(ImapScript::default()).await;
    // `.test` never resolves (RFC 6761): completing a whole session proves no lookup happened.
    let batch = fetch_new_pinned(
        &config(&server, "mail.spot.test"),
        &pinned(&server),
        None,
        FetchLimits::default(),
    )
    .await
    .must();
    assert!(batch.messages.is_empty());
    assert_eq!(server.connections(), 1);
}

#[tokio::test]
async fn an_unresolvable_name_is_reached_through_its_pinned_address() {
    let server = spawn_tls().await;
    // A name that can never resolve, and a certificate that is not valid for it: the failure is
    // the TLS name check, on a connection that was made, not a lookup failure.
    let error = fetch_new_pinned(
        &config(&server, "mail.never-resolves.invalid"),
        &pinned(&server),
        None,
        FetchLimits::default(),
    )
    .await
    .must_err();
    assert!(
        matches!(&error, MailError::Transient(TransientFailure::TlsHandshake)),
        "got {error:?}"
    );
    assert_eq!(server.connections(), 1);
}

#[tokio::test]
async fn an_empty_pinned_list_is_refused_before_any_socket_call() {
    let server = spawn_tls().await;
    let error = fetch_new_pinned(
        &config(&server, "mail.spot.test"),
        &[],
        None,
        FetchLimits::default(),
    )
    .await
    .must_err();
    assert!(is_pinned_refusal(&error), "got {error:?}");
    assert!(!error.is_retryable());
    assert_eq!(server.connections(), 0);
}

#[tokio::test]
async fn a_host_name_without_pinned_addresses_is_refused_before_any_socket_call() {
    let server = spawn_tls().await;
    // The unpinned entry point accepts a loopback IP literal only; a name would need a lookup.
    for host in ["mail.spot.test", "localhost", "10.1.2.3", "93.184.216.34"] {
        let error = fetch_new(&config(&server, host), None, FetchLimits::default())
            .await
            .must_err();
        assert!(is_pinned_refusal(&error), "{host}: got {error:?}");
    }
    assert_eq!(server.connections(), 0);
}

#[tokio::test]
async fn the_tls_name_is_the_host_not_the_pinned_address() {
    let server = spawn_tls().await;
    server.set_script(ImapScript::default()).await;
    // The peer is an IP, the certificate is valid for `mail.spot.test` and for the IP itself.
    // Verification must use the configured host: the right name passes, another name fails even
    // though the pinned IP would have matched the certificate.
    fetch_new_pinned(
        &config(&server, "mail.spot.test"),
        &pinned(&server),
        None,
        FetchLimits::default(),
    )
    .await
    .must();
    let error = fetch_new_pinned(
        &config(&server, "other.spot.test"),
        &pinned(&server),
        None,
        FetchLimits::default(),
    )
    .await
    .must_err();
    assert!(
        matches!(&error, MailError::Transient(TransientFailure::TlsHandshake)),
        "got {error:?}"
    );
}

#[tokio::test]
async fn pinned_addresses_are_tried_in_order() {
    let server = spawn_tls().await;
    server.set_script(ImapScript::default()).await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").must();
    let dead = listener.local_addr().must();
    drop(listener);
    let addrs = vec![dead, pinned(&server)[0]];
    fetch_new_pinned(
        &config(&server, "mail.spot.test"),
        &addrs,
        None,
        FetchLimits::default(),
    )
    .await
    .must();
    assert_eq!(server.connections(), 1);
}
