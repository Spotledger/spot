//! End-to-end envelopes and error classification against a scripted fake ESMTP server
//! (WP-PLT-11.T1).

mod support;

use std::net::SocketAddr;
use std::time::{Duration, SystemTime};

use spot_mail::error::{MailError, PermanentFailure, TransientFailure};
use spot_mail::message::{Mailbox, OutgoingMessage, Secret};
use spot_mail::mime::build_mime_with_id;
use spot_mail::smtp::{Credentials, SmtpConfig, SmtpTransport, TransportSecurity};
use spot_mail::transport::MailTransport;
use support::{FakeServer, Must, MustErr, Reply, Script, spawn_plain, spawn_starttls, spawn_tls};

/// A transport pointed at the fake server.
fn config(server: &FakeServer, pool_size: usize, timeout: Duration) -> SmtpConfig {
    SmtpConfig {
        host: "127.0.0.1".to_owned(),
        port: server.port(),
        security: TransportSecurity::Plain,
        credentials: Some(Credentials {
            user: support::FAKE_USER.to_owned(),
            secret: Secret::new(support::FAKE_PASSWORD),
        }),
        timeout,
        pool_size,
        allow_insecure: false,
        root_certificate_pem: None,
        pinned: Some(vec![SocketAddr::from(([127, 0, 0, 1], server.port()))]),
    }
}

fn transport(server: &FakeServer, pool_size: usize) -> SmtpTransport {
    SmtpTransport::new(config(server, pool_size, Duration::from_secs(5))).must()
}

/// A TLS or STARTTLS transport trusting the server's self-signed certificate.
fn tls_transport(
    server: &FakeServer,
    security: TransportSecurity,
    trust_pem: bool,
) -> SmtpTransport {
    SmtpTransport::new(SmtpConfig {
        host: "127.0.0.1".to_owned(),
        port: server.port(),
        security,
        credentials: Some(Credentials {
            user: support::FAKE_USER.to_owned(),
            secret: Secret::new(support::FAKE_PASSWORD),
        }),
        timeout: Duration::from_secs(5),
        pool_size: 0,
        allow_insecure: false,
        root_certificate_pem: trust_pem
            .then(|| server.host_certificate_pem().must())
            .map(str::as_bytes)
            .map(<[u8]>::to_vec),
        pinned: Some(vec![SocketAddr::from(([127, 0, 0, 1], server.port()))]),
    })
    .must()
}

fn box_(address: &str) -> Mailbox {
    Mailbox::new(None, address).must()
}

/// A fixed `Date` for every fixture message: the transport stamps `Date` with the build time
/// when the message carries none, so a test that rebuilds the MIME after the send to compare
/// with the recorded DATA would race on a second boundary (review comment). An
/// explicit date makes every send and rebuild produce byte-identical MIME.
fn fixed_date() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

/// A message with the given recipients, a subject and a text body.
fn message(recipients: &[&str]) -> OutgoingMessage {
    let mut message = OutgoingMessage::new(box_("sender@spot.test"), "Spot test message");
    for address in recipients {
        message.to.push(box_(address));
    }
    message.text = Some("Hello from spot_mail.".to_owned());
    message.date = Some(fixed_date());
    message
}

fn sent(addresses: &[spot_mail::Address]) -> Vec<String> {
    addresses.iter().map(ToString::to_string).collect()
}

/// SMTP's dot-terminator frame guarantees a final CRLF after the last body line; the MIME
/// document `build_mime` produces may end without one, so trim the recorded side before
/// comparing golden bodies.
fn trim_trailing_crlf(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

#[tokio::test]
async fn delivers_and_records_the_envelope() {
    let server = spawn_plain().await;
    let transport = transport(&server, 0);
    let sent_message = message(&["ada@dest.test", "bob@dest.test"]);

    let receipt = transport.send(&sent_message).await.must();

    assert_eq!(receipt.accepted.len(), 2);
    assert_eq!(
        sent(&receipt.accepted),
        vec!["ada@dest.test", "bob@dest.test"]
    );
    assert!(receipt.rejected.is_empty());
    let recording = server.last_recording().await;
    assert_eq!(recording.mail_from.as_deref(), Some("sender@spot.test"));
    assert_eq!(
        recording.accepted_rcpt,
        vec!["ada@dest.test", "bob@dest.test"]
    );
    assert!(recording.authenticated);
    // Pooling disabled: the connection is closed politely and only once.
    assert!(recording.commands.iter().any(|c| c == "QUIT"));
    assert_eq!(server.connections(), 1);
    // The DATA content is exactly the MIME bytes, dot-unescaped by the server.
    let expected = String::from_utf8(build_mime_with_id(&sent_message, &receipt.message_id)).must();
    assert_eq!(trim_trailing_crlf(&recording.body.must()), expected);
}

#[tokio::test]
async fn reuses_a_pooled_connection() {
    let server = spawn_plain().await;
    let transport = transport(&server, 1);
    let first = message(&["ada@dest.test"]);
    let second = message(&["bob@dest.test"]);

    let _ = transport.send(&first).await.must();
    let _ = transport.send(&second).await.must();

    assert_eq!(
        server.connections(),
        1,
        "no second TCP connection was opened"
    );
    let recordings = server.take_recordings().await;
    // One recorded session carries both sends: a pooled session never closes between them.
    assert_eq!(recordings.len(), 1, "both sends share one recorded session");
    let session = &recordings[0];
    assert!(session.commands.iter().any(|c| c.starts_with("EHLO")));
    assert!(session.commands.iter().any(|c| c.starts_with("AUTH PLAIN")));
    assert_eq!(
        session
            .commands
            .iter()
            .filter(|c| c.starts_with("MAIL"))
            .count(),
        2,
        "both envelopes issued MAIL on the pooled session"
    );
    assert!(
        session.commands.iter().all(|c| c != "QUIT"),
        "a pooled connection is never closed after DATA"
    );
}

#[tokio::test]
async fn retries_a_stale_pooled_connection_before_data() {
    let server = spawn_plain().await;
    let transport = transport(&server, 1);
    let first = message(&["ada@dest.test"]);
    let second = message(&["bob@dest.test"]);

    let _ = transport.send(&first).await.must();
    // The server drops the now-idle connection during the next MAIL, like a restart would.
    server
        .set_script(Script {
            hangup_after_mail: true,
            ..Script::default()
        })
        .await;
    let receipt = transport.send(&second).await.must();

    assert_eq!(sent(&receipt.accepted), vec!["bob@dest.test"]);
    assert_eq!(
        server.connections(),
        2,
        "one fresh connection backs the retried send"
    );
    let recordings = server.take_recordings().await;
    // The pooled session carried the first envelope and was cut during the second MAIL.
    let hung = &recordings[0];
    assert!(
        hung.body.is_some(),
        "send one delivered on the pooled session"
    );
    assert_eq!(
        hung.commands
            .iter()
            .filter(|c| c.starts_with("MAIL"))
            .count(),
        2,
        "both senders hit the same pooled session before the hangup"
    );
    // The retry opened its own session, re-authenticated and completed the envelope.
    let retry = &recordings[1];
    assert!(
        retry.commands.iter().any(|c| c.starts_with("EHLO")),
        "the retry re-handshakes"
    );
    assert!(
        retry.commands.iter().any(|c| c.starts_with("AUTH PLAIN")),
        "the retry re-authenticates"
    );
    let expected = String::from_utf8(build_mime_with_id(&second, &receipt.message_id)).must();
    assert_eq!(trim_trailing_crlf(retry.body.as_deref().must()), expected);
}

#[tokio::test]
async fn reports_partial_rejection_and_keeps_the_session() {
    let server = spawn_plain().await;
    server
        .set_script(Script {
            reject_rcpt: vec![(
                "bob@dest.test".to_owned(),
                Reply::new(550, "5.1.1 mailbox unavailable"),
            )],
            ..Script::default()
        })
        .await;
    let transport = transport(&server, 0);
    let sent_message = message(&["ada@dest.test", "bob@dest.test", "carol@dest.test"]);

    let receipt = transport.send(&sent_message).await.must();

    assert_eq!(
        sent(&receipt.accepted),
        vec!["ada@dest.test", "carol@dest.test"]
    );
    assert_eq!(receipt.rejected.len(), 1);
    assert_eq!(receipt.rejected[0].0.to_string(), "bob@dest.test");
    assert_eq!(receipt.rejected[0].1.code, 550);
    let recording = server.last_recording().await;
    // The session stayed alive after the rejection: DATA was still sent.
    assert_eq!(
        recording.accepted_rcpt,
        vec!["ada@dest.test", "carol@dest.test"]
    );
    assert_eq!(
        recording.rejected_rcpt,
        vec![("bob@dest.test".to_owned(), 550)]
    );
    assert!(recording.body.is_some(), "DATA still happens");
}

#[tokio::test]
async fn fails_permanently_when_every_recipient_is_rejected() {
    let server = spawn_plain().await;
    server
        .set_script(Script {
            reject_rcpt: vec![(
                "ada@dest.test".to_owned(),
                Reply::new(550, "5.1.1 unknown user"),
            )],
            ..Script::default()
        })
        .await;
    let transport = transport(&server, 0);

    let error = transport
        .send(&message(&["ada@dest.test"]))
        .await
        .must_err();

    assert!(
        matches!(
            &error,
            MailError::Permanent(PermanentFailure::Reply(reply)) if reply.code == 550
        ),
        "got {error:?}"
    );
    assert!(!error.is_retryable());
    assert!(server.last_recording().await.body.is_none(), "no DATA");
}

#[tokio::test]
async fn classifies_a_mail_5xx_as_permanent() {
    let server = spawn_plain().await;
    server
        .set_script(Script {
            mail_reply: Some(Reply::new(530, "5.7.0 authentication required")),
            ..Script::default()
        })
        .await;
    let transport = transport(&server, 0);

    let error = transport
        .send(&message(&["ada@dest.test"]))
        .await
        .must_err();

    assert!(
        matches!(
            &error,
            MailError::Permanent(PermanentFailure::Reply(reply)) if reply.code == 530
        ),
        "got {error:?}"
    );
    assert!(!error.is_retryable());
    assert_eq!(error.reply().map(|reply| reply.code), Some(530));
}

#[tokio::test]
async fn classifies_a_data_4xx_as_transient() {
    let server = spawn_plain().await;
    server
        .set_script(Script {
            data_reply: Some(Reply::new(451, "4.7.0 local error, try later")),
            ..Script::default()
        })
        .await;
    let transport = transport(&server, 0);

    let error = transport
        .send(&message(&["ada@dest.test"]))
        .await
        .must_err();

    assert!(
        matches!(
            &error,
            MailError::Transient(TransientFailure::Reply(reply)) if reply.code == 451
        ),
        "got {error:?}"
    );
    assert!(error.is_retryable());
    assert!(server.last_recording().await.body.is_none(), "no body sent");
}

#[tokio::test]
async fn classifies_refused_credentials_as_permanent() {
    let server = spawn_plain().await;
    server
        .set_script(Script {
            reject_auth: true,
            ..Script::default()
        })
        .await;
    let transport = transport(&server, 0);

    let error = transport
        .send(&message(&["ada@dest.test"]))
        .await
        .must_err();

    assert!(
        matches!(
            &error,
            MailError::Permanent(PermanentFailure::Authentication { reply: Some(reply) })
                if reply.code == 535
        ),
        "got {error:?}"
    );
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn classifies_a_silent_server_as_a_timeout() {
    let server = spawn_plain().await;
    server
        .set_script(Script {
            no_reply: true,
            ..Script::default()
        })
        .await;
    let transport = SmtpTransport::new(config(&server, 0, Duration::from_millis(400))).must();

    let error = transport
        .send(&message(&["ada@dest.test"]))
        .await
        .must_err();

    assert!(
        matches!(&error, MailError::Transient(TransientFailure::Timeout)),
        "got {error:?}"
    );
    assert!(error.is_retryable());
}

#[tokio::test]
async fn classifies_connection_refused_as_transient() {
    // Bind a listener only to learn a free port, then close it: nothing is listening.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").must();
    let port = listener.local_addr().must().port();
    drop(listener);

    let transport = SmtpTransport::new(SmtpConfig {
        host: "127.0.0.1".to_owned(),
        port,
        security: TransportSecurity::Plain,
        credentials: Some(Credentials {
            user: support::FAKE_USER.to_owned(),
            secret: Secret::new(support::FAKE_PASSWORD),
        }),
        timeout: Duration::from_millis(400),
        pool_size: 0,
        allow_insecure: false,
        root_certificate_pem: None,
        pinned: Some(vec![SocketAddr::from(([127, 0, 0, 1], port))]),
    })
    .must();

    let error = transport
        .send(&message(&["ada@dest.test"]))
        .await
        .must_err();

    // Windows keeps retrying SYNs to a closed loopback port for about two seconds before it
    // reports a refusal, so the 400 ms phase timeout can fire first there. Either way the
    // failure is transient and retryable, which is the property under test.
    let expected = matches!(
        &error,
        MailError::Transient(TransientFailure::ConnectionRefused)
    ) || (cfg!(windows)
        && matches!(&error, MailError::Transient(TransientFailure::Timeout)));
    assert!(expected, "got {error:?}");
    assert!(error.is_retryable());
}

#[tokio::test]
async fn sends_utf8_content_with_8bitmime() {
    let server = spawn_plain().await;
    let transport = transport(&server, 0);
    let mut sent_message = message(&["ada@dest.test"]);
    sent_message.text = Some("Grüße aus der Wüste! ☃".to_owned());

    let receipt = transport.send(&sent_message).await.must();

    let recording = server.last_recording().await;
    // The MAIL command declares the 8-bit body because the server advertises 8BITMIME.
    let mail = recording
        .commands
        .iter()
        .find(|c| c.starts_with("MAIL"))
        .must();
    assert!(mail.contains("BODY=8BITMIME"), "got {mail:?}");
    // The UTF-8 body survived the wire unchanged.
    let expected = String::from_utf8(build_mime_with_id(&sent_message, &receipt.message_id)).must();
    assert_eq!(trim_trailing_crlf(&recording.body.must()), expected);
}

#[tokio::test]
async fn delivers_over_implicit_tls() {
    let server = spawn_tls().await;
    let transport = tls_transport(&server, TransportSecurity::Tls, true);
    let sent_message = message(&["ada@dest.test"]);

    let receipt = transport.send(&sent_message).await.must();

    assert_eq!(sent(&receipt.accepted), vec!["ada@dest.test"]);
    let recording = server.last_recording().await;
    assert!(recording.authenticated);
    let expected = String::from_utf8(build_mime_with_id(&sent_message, &receipt.message_id)).must();
    assert_eq!(trim_trailing_crlf(&recording.body.must()), expected);
}

#[tokio::test]
async fn upgrades_via_starttls() {
    let server = spawn_starttls().await;
    let transport = tls_transport(&server, TransportSecurity::StartTls, true);
    let sent_message = message(&["ada@dest.test"]);

    let receipt = transport.send(&sent_message).await.must();

    assert_eq!(sent(&receipt.accepted), vec!["ada@dest.test"]);
    let recording = server.last_recording().await;
    // EHLO was answered on clear text, the rest arrived through TLS.
    assert!(recording.commands.iter().any(|c| c == "STARTTLS"));
    assert!(recording.authenticated);
    let expected = String::from_utf8(build_mime_with_id(&sent_message, &receipt.message_id)).must();
    assert_eq!(trim_trailing_crlf(&recording.body.must()), expected);
}

#[tokio::test]
async fn rejects_an_untrusted_tls_server_as_transient() {
    let server = spawn_tls().await;
    // The system roots do not trust a self-signed server certificate.
    let transport = tls_transport(&server, TransportSecurity::Tls, false);

    let error = transport
        .send(&message(&["ada@dest.test"]))
        .await
        .must_err();

    assert!(
        matches!(&error, MailError::Transient(TransientFailure::TlsHandshake)),
        "got {error:?}"
    );
    assert!(error.is_retryable());
}

#[tokio::test]
async fn refuses_plain_security_off_loopback() {
    let config = SmtpConfig {
        host: "mail.example.com".to_owned(),
        port: 25,
        security: TransportSecurity::Plain,
        credentials: None,
        timeout: Duration::from_secs(5),
        pool_size: 0,
        allow_insecure: false,
        root_certificate_pem: None,
        pinned: Some(vec![SocketAddr::from(([93, 184, 216, 34], 25))]),
    };
    let error = SmtpTransport::new(config).must_err();

    assert!(
        matches!(&error, MailError::Permanent(PermanentFailure::Client(reason))
            if reason.contains("Plain SMTP security is refused")),
        "got {error:?}"
    );
    // The escape hatch allows it.
    let insecure = SmtpConfig {
        host: "mail.example.com".to_owned(),
        port: 25,
        security: TransportSecurity::Plain,
        credentials: None,
        timeout: Duration::from_secs(5),
        pool_size: 0,
        allow_insecure: true,
        root_certificate_pem: None,
        pinned: Some(vec![SocketAddr::from(([93, 184, 216, 34], 25))]),
    };
    assert!(SmtpTransport::new(insecure).is_ok());
}

#[tokio::test]
async fn validates_the_message_before_touching_the_network() {
    let server = spawn_plain().await;
    let transport = transport(&server, 0);

    let error = transport
        .send(&OutgoingMessage::new(
            box_("sender@spot.test"),
            "nobody to send to",
        ))
        .await
        .must_err();

    assert!(
        matches!(&error, MailError::Permanent(PermanentFailure::InvalidMessage(reason))
            if reason.contains("at least one recipient")),
        "got {error:?}"
    );
    assert_eq!(server.connections(), 0, "the client never connected");
}

#[tokio::test]
async fn reports_the_id_that_went_on_the_wire() {
    let server = spawn_plain().await;
    let transport = transport(&server, 0);
    let mut sent_message = message(&["ada@dest.test"]);
    sent_message.message_id = Some("fixed-id@spot.test".to_owned());

    let receipt = transport.send(&sent_message).await.must();

    assert_eq!(receipt.message_id, "fixed-id@spot.test");
    let recording = server.last_recording().await;
    let body = recording.body.must();
    assert!(
        body.contains("Message-ID: <fixed-id@spot.test>"),
        "got header {body:?}"
    );
}

// --- Pinned addresses (WP-PLT-11.T4: DNS rebinding between the egress check and the
// connect). ---

/// A config that dials `pinned` and names `host` only for TLS.
fn pinned_config(
    host: &str,
    server: &FakeServer,
    security: TransportSecurity,
    trust_pem: bool,
) -> SmtpConfig {
    let mut config = config(server, 0, Duration::from_secs(5));
    config.host = host.to_owned();
    config.security = security;
    config.pinned = Some(vec![SocketAddr::from(([127, 0, 0, 1], server.port()))]);
    config.root_certificate_pem = trust_pem
        .then(|| server.host_certificate_pem().must())
        .map(str::as_bytes)
        .map(<[u8]>::to_vec);
    config
}

#[tokio::test]
async fn a_pinned_transport_never_resolves_the_host() {
    let server = spawn_plain().await;
    // `.invalid` can never resolve (RFC 6761): reaching the server proves no lookup happened.
    let transport = SmtpTransport::new(pinned_config(
        "mail.never-resolves.invalid",
        &server,
        TransportSecurity::Plain,
        false,
    ))
    .must();

    let receipt = transport.send(&message(&["ada@dest.test"])).await.must();

    assert_eq!(sent(&receipt.accepted), vec!["ada@dest.test"]);
    assert_eq!(server.connections(), 1);
    assert_eq!(transport.pinned().map(<[SocketAddr]>::len), Some(1));
}

#[tokio::test]
async fn pinned_tls_verifies_the_certificate_against_the_host_name() {
    let server = spawn_tls().await;
    // The peer is an IP; the certificate is valid for `mail.spot.test` and must be checked
    // against that name.
    let good = SmtpTransport::new(pinned_config(
        "mail.spot.test",
        &server,
        TransportSecurity::Tls,
        true,
    ))
    .must();
    good.send(&message(&["ada@dest.test"])).await.must();

    let wrong = SmtpTransport::new(pinned_config(
        "other.spot.test",
        &server,
        TransportSecurity::Tls,
        true,
    ))
    .must();
    let error = wrong.send(&message(&["ada@dest.test"])).await.must_err();
    assert!(
        matches!(&error, MailError::Transient(TransientFailure::TlsHandshake)),
        "got {error:?}"
    );
}

#[tokio::test]
async fn pinned_starttls_verifies_the_certificate_against_the_host_name() {
    let server = spawn_starttls().await;
    let good = SmtpTransport::new(pinned_config(
        "mail.spot.test",
        &server,
        TransportSecurity::StartTls,
        true,
    ))
    .must();
    good.send(&message(&["ada@dest.test"])).await.must();
    assert!(
        server
            .last_recording()
            .await
            .commands
            .iter()
            .any(|c| c == "STARTTLS")
    );

    let wrong = SmtpTransport::new(pinned_config(
        "other.spot.test",
        &server,
        TransportSecurity::StartTls,
        true,
    ))
    .must();
    let error = wrong.send(&message(&["ada@dest.test"])).await.must_err();
    assert!(
        matches!(&error, MailError::Transient(TransientFailure::TlsHandshake)),
        "got {error:?}"
    );
    // No credential reached the server on the failed session.
    assert!(!server.last_recording().await.authenticated);
}

#[tokio::test]
async fn pinned_addresses_are_tried_in_order() {
    let live = spawn_plain().await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").must();
    let dead_port = listener.local_addr().must().port();
    drop(listener);
    let mut config = pinned_config(
        "mail.never-resolves.invalid",
        &live,
        TransportSecurity::Plain,
        false,
    );
    config.pinned = Some(vec![
        SocketAddr::from(([127, 0, 0, 1], dead_port)),
        SocketAddr::from(([127, 0, 0, 1], live.port())),
    ]);
    let transport = SmtpTransport::new(config).must();

    let receipt = transport.send(&message(&["ada@dest.test"])).await.must();

    assert_eq!(sent(&receipt.accepted), vec!["ada@dest.test"]);
    assert_eq!(live.connections(), 1);
}

#[tokio::test]
async fn a_silent_pinned_server_times_out_per_phase() {
    let server = spawn_plain().await;
    server
        .set_script(Script {
            no_reply: true,
            ..Script::default()
        })
        .await;
    let mut config = pinned_config(
        "mail.never-resolves.invalid",
        &server,
        TransportSecurity::Plain,
        false,
    );
    config.timeout = Duration::from_millis(300);
    let transport = SmtpTransport::new(config).must();

    let error = transport
        .send(&message(&["ada@dest.test"]))
        .await
        .must_err();

    assert!(
        matches!(&error, MailError::Transient(TransientFailure::Timeout)),
        "got {error:?}"
    );
}

#[tokio::test]
async fn an_empty_pinned_list_is_refused_instead_of_falling_back_to_the_host() {
    let server = spawn_plain().await;
    let mut config = pinned_config("127.0.0.1", &server, TransportSecurity::Plain, false);
    config.pinned = Some(Vec::new());

    let error = SmtpTransport::new(config).must_err();

    assert!(
        matches!(&error, MailError::Permanent(PermanentFailure::Client(reason))
            if reason.contains("pinned address list is empty")),
        "got {error:?}"
    );
}

#[tokio::test]
async fn plain_security_is_judged_by_the_pinned_addresses_not_the_host_name() {
    let server = spawn_plain().await;
    // A host that looks local but is pinned to a routable address is not loopback.
    let mut config = pinned_config("localhost", &server, TransportSecurity::Plain, false);
    config.pinned = Some(vec![SocketAddr::from(([93, 184, 216, 34], 25))]);
    let error = SmtpTransport::new(config).must_err();
    assert!(
        matches!(&error, MailError::Permanent(PermanentFailure::Client(reason))
            if reason.contains("Plain SMTP security is refused")),
        "got {error:?}"
    );
}

#[tokio::test]
async fn debug_output_shows_no_credential_for_a_pinned_transport() {
    let server = spawn_plain().await;
    let transport = SmtpTransport::new(pinned_config(
        "mail.spot.test",
        &server,
        TransportSecurity::Plain,
        false,
    ))
    .must();

    let debug = format!("{transport:?}");

    assert!(!debug.contains(support::FAKE_PASSWORD), "{debug}");
}

#[tokio::test]
async fn a_config_without_pinned_addresses_is_refused_before_any_socket_call() {
    let server = spawn_plain().await;
    // Nothing dials: the constructor refuses, and the server never sees a connection.
    for host in ["127.0.0.1", "mail.never-resolves.invalid"] {
        let mut config = pinned_config(host, &server, TransportSecurity::Plain, false);
        config.pinned = None;
        let error = SmtpTransport::new(config).must_err();
        assert!(
            matches!(&error, MailError::Permanent(PermanentFailure::Client(reason))
                if reason.contains("pinned addresses are required")),
            "got {error:?}"
        );
        assert!(!error.is_retryable());
    }
    assert_eq!(server.connections(), 0);
}
