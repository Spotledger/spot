//! The mail port dials only the addresses the egress guard authorised (DNS rebinding).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spot_app_api::prelude::Clock;
use spot_bpm_integration::{
    EgressMailPort, IntegrationError, MailPort, PinnedTransportFactory, RecordingFactory,
    SmtpPinnedFactory,
};
use spot_egress::{Authorized, EgressPolicy, Guard, InMemorySink, TestResolver};
use spot_mail::{
    CaptureTransport, MailError, MailTransport, Mailbox, OutgoingMessage, SmtpConfig,
    TransportSecurity,
};
use time::OffsetDateTime;

struct FixedClock;

impl Clock for FixedClock {
    fn now_utc(&self) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
}

const HOST: &str = "smtp.example.test";
const PUBLIC: &str = "93.184.216.34";
const PUBLIC_TWO: &str = "93.184.216.35";
const PRIVATE: &str = "10.1.2.3";

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

fn sock(text: &str) -> SocketAddr {
    SocketAddr::new(ip(text), 587)
}

fn message() -> OutgoingMessage {
    let mut message = OutgoingMessage::new(
        Mailbox::new(None, "orders@acme.test").unwrap(),
        "Order SO-1",
    );
    message.to = vec![Mailbox::new(None, "jane@example.test").unwrap()];
    message.text = Some("Thanks.".to_owned());
    message
}

struct Rig {
    resolver: Arc<TestResolver>,
    capture: Arc<CaptureTransport>,
    factory: Arc<RecordingFactory>,
    port: EgressMailPort,
}

/// A port whose resolver answers `answers` in turn for [`HOST`].
fn rig(answers: Vec<Vec<IpAddr>>) -> Rig {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response(HOST, answers);
    let guard = Arc::new(Guard::new(
        EgressPolicy::default(),
        resolver.clone(),
        Arc::new(InMemorySink::new()),
        Arc::new(FixedClock),
    ));
    let capture = Arc::new(CaptureTransport::new());
    let factory = Arc::new(RecordingFactory::new(capture.clone()));
    let port = EgressMailPort::new(guard, factory.clone(), HOST, 587, "acme");
    Rig {
        resolver,
        capture,
        factory,
        port,
    }
}

#[tokio::test]
async fn the_factory_receives_exactly_the_addresses_the_guard_authorised() {
    let rig = rig(vec![vec![ip(PUBLIC_TWO), ip(PUBLIC)]]);
    rig.port.send(&message()).await.unwrap();

    // The set the guard vetted, sorted: socket addresses, never the host name.
    assert_eq!(
        rig.factory.built(),
        vec![vec![sock(PUBLIC), sock(PUBLIC_TWO)]]
    );
    assert_eq!(rig.capture.messages().len(), 1);
    assert_eq!(rig.resolver.call_count(HOST), 1, "one lookup, by the guard");
}

#[tokio::test]
async fn an_authorisation_with_no_addresses_sends_nothing() {
    let rig = rig(vec![]);
    let empty = Authorized {
        addrs: Vec::new(),
        original_host: HOST.to_owned(),
    };
    let error = rig
        .port
        .send_authorized(&empty, &message())
        .await
        .unwrap_err();
    assert_eq!(error.code(), "bpm_integration.egress_denied");
    assert!(
        matches!(&error, IntegrationError::EgressDenied { code } if code == "egress.pinned_required"),
        "{error:?}"
    );
    assert!(rig.factory.built().is_empty(), "no transport was built");
    assert!(rig.capture.messages().is_empty(), "nothing was sent");
}

#[tokio::test]
async fn a_host_that_rebinds_after_the_check_is_never_dialled_at_the_private_address() {
    // Public on the first lookup, private on the second.
    let rig = rig(vec![vec![ip(PUBLIC)], vec![ip(PRIVATE)]]);

    // The first send dials only the vetted public address.
    rig.port.send(&message()).await.unwrap();
    assert_eq!(rig.factory.built(), vec![vec![sock(PUBLIC)]]);

    // The second send re-authorises, sees the private answer and is refused: no new transport,
    // no second message, and the private address never reaches the factory.
    let error = rig.port.send(&message()).await.unwrap_err();
    assert_eq!(error.code(), "bpm_integration.egress_denied");
    assert_eq!(rig.factory.built(), vec![vec![sock(PUBLIC)]]);
    assert_eq!(rig.capture.messages().len(), 1);
    assert_eq!(
        rig.resolver.call_count(HOST),
        2,
        "the guard looked up twice; the transport never did"
    );
}

#[tokio::test]
async fn a_transport_is_reused_for_the_same_set_and_replaced_for_another() {
    let rig = rig(vec![
        vec![ip(PUBLIC), ip(PUBLIC_TWO)],
        vec![ip(PUBLIC_TWO), ip(PUBLIC)],
        vec![ip(PUBLIC)],
    ]);
    for _ in 0..3 {
        rig.port.send(&message()).await.unwrap();
    }
    assert_eq!(
        rig.factory.built(),
        vec![vec![sock(PUBLIC), sock(PUBLIC_TWO)], vec![sock(PUBLIC)]],
        "the same set in another order is the same transport; a different set builds a new one"
    );
}

/// A factory that fails, to prove a build error is a mail error and nothing is sent.
struct Failing;

impl PinnedTransportFactory for Failing {
    fn build(&self, _: Vec<SocketAddr>) -> Result<Arc<dyn MailTransport>, MailError> {
        Err(MailError::Permanent(spot_mail::PermanentFailure::Client(
            "bad configuration".to_owned(),
        )))
    }
}

#[tokio::test]
async fn a_factory_that_cannot_build_fails_the_send() {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response(HOST, vec![vec![ip(PUBLIC)]]);
    let guard = Arc::new(Guard::new(
        EgressPolicy::default(),
        resolver,
        Arc::new(InMemorySink::new()),
        Arc::new(FixedClock),
    ));
    let port = EgressMailPort::new(guard, Arc::new(Failing), HOST, 587, "acme");
    let error = port.send(&message()).await.unwrap_err();
    assert_eq!(error.code(), "bpm_integration.mail_permanent");
}

#[tokio::test]
async fn the_production_factory_never_looks_up_the_host() {
    // A local listener stands for the vetted address. The host name is one that can never
    // resolve (RFC 6761): connecting proves the transport dialled the address and did not look
    // the name up.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // A greeting, then hang up: enough to prove the connection, not a full session.
        tokio::io::AsyncWriteExt::write_all(&mut socket, b"554 go away\r\n")
            .await
            .unwrap();
    });
    let factory = SmtpPinnedFactory::new(SmtpConfig {
        host: "mail.never-resolves.invalid".to_owned(),
        port: addr.port(),
        security: TransportSecurity::Plain,
        credentials: None,
        timeout: Duration::from_secs(5),
        pool_size: 0,
        allow_insecure: false,
        root_certificate_pem: None,
        pinned: None,
    });
    let transport = factory.build(vec![addr]).unwrap();
    let error = transport.send(&message()).await.unwrap_err();
    tokio::time::timeout(Duration::from_secs(5), accepted)
        .await
        .expect("the listener saw a connection")
        .unwrap();
    // The server refused the session; a failed lookup would have been a transient resolution
    // error that never reached it.
    assert!(
        !error.to_string().to_lowercase().contains("dns")
            && !error.to_string().to_lowercase().contains("resolve"),
        "{error}"
    );
}
