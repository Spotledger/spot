//! The real SMTP transport (WP-PLT-11.T1), built on lettre's async client.
//!
//! Envelope commands are driven by hand per recipient: MAIL, one RCPT per recipient (failures are
//! collected and never abort the session), then DATA. That is what makes partial rejection an
//! ordinary receipt instead of an error, which lettre's high-level transports cannot produce
//! (they abort the session on the first rejected recipient). Connections are pooled per
//! transport, honoring `pool_size`. TLS: implicit [`TransportSecurity::Tls`],
//! [`TransportSecurity::StartTls`] when the server offers it, or [`TransportSecurity::Plain`] —
//! which is refused for non-loopback hosts unless `allow_insecure` is set (see
//! `docs/blueprint/platform.md`, outgoing mail).

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use lettre::Address;
use lettre::transport::smtp::Error as LettreSmtpError;
use lettre::transport::smtp::authentication::{Credentials as LettreCredentials, Mechanism};
use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};
use lettre::transport::smtp::commands::{Data, Mail, Rcpt};
use lettre::transport::smtp::extension::{ClientId, Extension, MailBodyParameter, MailParameter};

use crate::error::{MailError, PermanentFailure, SmtpReply, TransientFailure};
use crate::message::{OutgoingMessage, Secret};
use crate::mime::build_mime_with_id;
use crate::transport::{MailTransport, SendReceipt};

/// How the connection to the server is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportSecurity {
    /// SMTP over an implicitly encrypted connection, usually on the submissions port (465).
    Tls,
    /// Plain SMTP upgraded to TLS when the server offers `STARTTLS`; fails when it does not.
    StartTls,
    /// Unencrypted SMTP; only sensible for loopback destinations and tests.
    Plain,
}

/// SMTP credentials for `AUTH PLAIN` / `AUTH LOGIN`.
#[derive(Debug, Clone)]
pub struct Credentials {
    /// The user name, usually the account address.
    pub user: String,
    /// The password or app-specific secret; redacted in `Debug` and zeroized on drop.
    pub secret: Secret,
}

/// Static configuration for [`SmtpTransport`].
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    /// The server host, a name or an address.
    pub host: String,
    /// The server port.
    pub port: u16,
    /// How the connection is protected; `Plain` is refused for non-loopback hosts unless
    /// `allow_insecure`.
    pub security: TransportSecurity,
    /// Credentials, when the server requires authentication.
    pub credentials: Option<Credentials>,
    /// Timeout for each transport phase: the TCP dial, the greeting/EHLO exchange, the TLS
    /// handshake, AUTH, and one full envelope drive (MAIL/RCPT/DATA). Lettre's tokio1 client
    /// applies its timeout only to the dial, so without per-phase wrapping a server that
    /// accepts the connection and then goes silent would hang a send forever.
    pub timeout: Duration,
    /// Maximum idle connections kept in the pool; 0 disables pooling.
    pub pool_size: usize,
    /// Escapes the `Plain` refusal for non-loopback destinations, explicitly opting out of the
    /// platform rule.
    pub allow_insecure: bool,
    /// The PEM of a CA or self-signed certificate the server must be trusted by, replacing the
    /// default certificate store.
    pub root_certificate_pem: Option<Vec<u8>>,
    /// The socket addresses to connect to, in order, when the destination was already vetted
    /// (the egress guard's `Authorized::socket_addrs`). When set, the transport dials these
    /// addresses only and never resolves `host`; `host` then serves only as the TLS server name
    /// (SNI and certificate verification). This is what closes the DNS rebinding window between
    /// the egress check and the connect. `None` resolves `host` at connect time (tests, loopback
    /// development); production wiring refuses that. `Some` of an empty list is invalid.
    pub pinned: Option<Vec<SocketAddr>>,
}

impl SmtpConfig {
    /// Validates the configuration: `Plain` security is refused for non-loopback destinations
    /// unless `allow_insecure` is set (the destination is the pinned addresses when set,
    /// otherwise the host), and a pinned list must not be empty.
    pub fn validate(&self) -> Result<(), MailError> {
        if self.pinned.as_ref().is_some_and(Vec::is_empty) {
            return Err(PermanentFailure::Client(
                "the pinned address list is empty; connect nowhere rather than resolve the host"
                    .to_owned(),
            )
            .into());
        }
        let loopback = match &self.pinned {
            Some(addrs) => addrs.iter().all(|addr| addr.ip().is_loopback()),
            None => is_loopback_host(&self.host),
        };
        match self.security {
            TransportSecurity::Plain if !loopback && !self.allow_insecure => {
                Err(PermanentFailure::Client(format!(
                    "Plain SMTP security is refused for non-loopback host {host:?}; use StartTls or Tls, or set allow_insecure",
                    host = self.host
                ))
                .into())
            }
            _ => Ok(()),
        }
    }
}

/// True when the host is `localhost` or a loopback address, with optional v6 brackets.
fn is_loopback_host(host: &str) -> bool {
    match host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
    {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

/// Builds the TLS parameters: trust the provided root certificate when set, otherwise the
/// system roots; never refuse to start when `allow_insecure` accepts any certificate.
fn build_tls(config: &SmtpConfig) -> Result<TlsParameters, MailError> {
    let mut builder = TlsParameters::builder(config.host.clone());
    if let Some(pem) = &config.root_certificate_pem {
        let certificate =
            lettre::transport::smtp::client::Certificate::from_pem(pem).map_err(|e| {
                PermanentFailure::InvalidMessage(format!("invalid root certificate PEM: {e}"))
            })?;
        builder =
            builder.certificate_store(lettre::transport::smtp::client::CertificateStore::None);
        builder = builder.add_root_certificate(certificate);
    }
    if config.security == TransportSecurity::Plain && config.allow_insecure {
        builder = builder.dangerous_accept_invalid_certs(true);
    }
    builder.build().map_err(|e| {
        PermanentFailure::InvalidMessage(format!("TLS parameters rejected: {e}")).into()
    })
}

/// The SMTP reply behind a lettre error, when the server produced one.
fn reply_of(error: &LettreSmtpError) -> Option<SmtpReply> {
    error
        .status()
        .map(|code| SmtpReply::new(u16::from(code), error.to_string()))
}

/// Maps a lettre error with a reply to the retry classification.
fn classified_reply(reply: SmtpReply) -> MailError {
    match reply.code / 100 {
        4 => TransientFailure::Reply(reply).into(),
        _ => PermanentFailure::Reply(reply).into(),
    }
}

/// Maps a lettre error to a [`MailError`] for the given protocol stage.
fn classify_error(error: &LettreSmtpError, stage: &str) -> MailError {
    if let Some(reply) = reply_of(error) {
        return classified_reply(reply);
    }
    if error.is_tls() {
        return TransientFailure::TlsHandshake.into();
    }
    if error.is_timeout() {
        return TransientFailure::Timeout.into();
    }
    if error.is_transport_shutdown() {
        return TransientFailure::ConnectionReset.into();
    }
    // A response the server never completed (EOF mid-reply, garbage, truncated line) means the
    // stream is gone or foreign; the connection cannot be trusted, so treat it as a reset. This
    // is what a server restart of an idle pooled connection surfaces as.
    if error.is_response() {
        return TransientFailure::ConnectionReset.into();
    }
    // Transport-level io faults, possibly buried several layers below the lettre kind (rustls
    // handshakes surface as `Kind::Client` errors wrapping an io error). The underlying kind
    // decides: a certificate problem may be a restarting server (retryable), a refusal or a
    // reset likewise. Before this walk these all looked like client bugs.
    if let Some(io) = source_io(error) {
        return match io.kind() {
            std::io::ErrorKind::InvalidData => TransientFailure::TlsHandshake.into(),
            std::io::ErrorKind::ConnectionRefused => TransientFailure::ConnectionRefused.into(),
            _ => TransientFailure::ConnectionReset.into(),
        };
    }
    if error.is_client() {
        return PermanentFailure::Client(format!("{stage}: {error}")).into();
    }
    PermanentFailure::Client(format!("{stage}: {error}")).into()
}

/// The deepest `std::io::Error` behind an error chain, when there is one.
fn source_io<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a std::io::Error> {
    let mut seen = Some(error);
    while let Some(current) = seen {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            return Some(io);
        }
        seen = std::error::Error::source(current);
    }
    None
}

/// Maps a lettre AUTH error: refused credentials or no compatible mechanism are permanent.
fn classify_auth_error(error: LettreSmtpError) -> MailError {
    if let Some(reply) = reply_of(&error) {
        return match reply.code / 100 {
            4 => TransientFailure::Reply(reply).into(),
            _ => PermanentFailure::Authentication { reply: Some(reply) }.into(),
        };
    }
    if error.is_client() {
        return PermanentFailure::Authentication { reply: None }.into();
    }
    classify_error(&error, "AUTH")
}

fn convert_credentials(credentials: &Credentials) -> LettreCredentials {
    LettreCredentials::new(
        credentials.user.clone(),
        credentials.secret.expose().to_string(),
    )
}

/// An SMTP transport over pooled lettre connections, with per-recipient envelope control.
/// The connections and TLS parameters are not printable, so `Debug` shows the configuration only.
pub struct SmtpTransport {
    config: SmtpConfig,
    hello_name: ClientId,
    tls: TlsParameters,
    idle_pool: Mutex<Vec<AsyncSmtpConnection>>,
}

impl fmt::Debug for SmtpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpTransport")
            .field("config", &self.config)
            .finish()
    }
}

impl SmtpTransport {
    /// Builds a transport, validating the configuration and the TLS parameters up front.
    ///
    /// The configuration must carry pinned addresses (`SmtpConfig::pinned`, the egress guard's
    /// `Authorized::socket_addrs`): a transport that would resolve `host` by name is refused
    /// here, before any socket call, so no code outside this crate can build one. Local
    /// development points the pinned list at the loopback address of its server.
    pub fn new(config: SmtpConfig) -> Result<Self, MailError> {
        if config.pinned.is_none() {
            return Err(PermanentFailure::Client(
                "pinned addresses are required; a transport never resolves the host by name"
                    .to_owned(),
            )
            .into());
        }
        config.validate()?;
        let hello_name = ClientId::Domain("spot".to_owned());
        let tls = build_tls(&config)?;
        Ok(Self {
            config,
            hello_name,
            tls,
            idle_pool: Mutex::new(Vec::new()),
        })
    }

    /// Pops an idle pooled connection, if any.
    fn take_idle(&self) -> Option<AsyncSmtpConnection> {
        self.idle().pop()
    }

    /// Returns a connection to the idle pool when there is room; otherwise closes it politely
    /// with QUIT (a connection that is not pooled must not be dropped mid-session).
    async fn release(&self, mut connection: AsyncSmtpConnection) {
        if self.has_pool_room() {
            self.idle().push(connection);
            return;
        }
        // Pool full (or pooling disabled): close politely so the server does not hang on us.
        let _ = connection.quit().await;
    }

    /// Whether the idle pool has room for one more connection. A concurrent release can slip
    /// past this between the check and the push; the pool then holds one connection over the
    /// cap until the next round, which is harmless.
    fn has_pool_room(&self) -> bool {
        let idle = self.idle();
        self.config.pool_size != 0 && idle.len() < self.config.pool_size
    }

    /// Locks the idle pool, recovering from poisoning (a poisoned pool holds healthy threads).
    fn idle(&self) -> MutexGuard<'_, Vec<AsyncSmtpConnection>> {
        self.idle_pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens a fresh connection: TCP (wrapped TLS for `Tls`), optional `STARTTLS` upgrade, then
    /// optional authentication. Every phase is bounded by the configured timeout.
    async fn connect(&self) -> Result<AsyncSmtpConnection, MailError> {
        let mut connection = self.dial().await?;
        if self.config.security == TransportSecurity::StartTls {
            let tls = self.tls.clone();
            let hello_name = self.hello_name.clone();
            self.within_timeout("STARTTLS", async {
                connection
                    .starttls(tls, &hello_name)
                    .await
                    .map_err(|e| classify_error(&e, "STARTTLS"))
            })
            .await?;
        }
        if let Some(credentials) = &self.config.credentials {
            let credentials = credentials.clone();
            self.within_timeout("auth", async {
                connection
                    .auth(
                        &[Mechanism::Plain, Mechanism::Login],
                        &convert_credentials(&credentials),
                    )
                    .await
                    .map_err(classify_auth_error)
            })
            .await?;
        }
        Ok(connection)
    }

    /// Dials the server. With pinned addresses, only those are tried, in order, each bounded by
    /// the timeout, and the host name is never resolved (a socket address needs no lookup; the
    /// host only names the server for TLS). Without them the host is resolved by the client.
    async fn dial(&self) -> Result<AsyncSmtpConnection, MailError> {
        let tls = match self.config.security {
            TransportSecurity::Tls => Some(self.tls.clone()),
            TransportSecurity::StartTls | TransportSecurity::Plain => None,
        };
        let timeout = self.config.timeout;
        let Some(pinned) = &self.config.pinned else {
            let server = format!("{}:{}", self.config.host, self.config.port);
            return self
                .within_timeout("connect", async {
                    AsyncSmtpConnection::connect_tokio1(
                        server.as_str(),
                        Some(timeout),
                        &self.hello_name,
                        tls,
                        None,
                    )
                    .await
                    .map_err(|e| classify_error(&e, "connect"))
                })
                .await;
        };
        let mut last: Option<MailError> = None;
        for addr in pinned {
            let attempt = self
                .within_timeout("connect", async {
                    AsyncSmtpConnection::connect_tokio1(
                        *addr,
                        Some(timeout),
                        &self.hello_name,
                        tls.clone(),
                        None,
                    )
                    .await
                    .map_err(|e| classify_error(&e, "connect"))
                })
                .await;
            match attempt {
                Ok(connection) => return Ok(connection),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| {
            PermanentFailure::Client("no pinned address to connect to".to_owned()).into()
        }))
    }

    /// The pinned addresses this transport dials, when it has them.
    pub fn pinned(&self) -> Option<&[SocketAddr]> {
        self.config.pinned.as_deref()
    }

    /// Bounds one SMTP phase with [`SmtpConfig::timeout`]: lettre's tokio1 client applies its
    /// timeout only to the TCP dial, so a server that accepts and then goes silent would hang
    /// the send forever without this. Elapsed time surfaces as
    /// [`TransientFailure::Timeout`], which the caller classifies as retryable.
    async fn within_timeout<T>(
        &self,
        phase: &'static str,
        phase_future: impl Future<Output = Result<T, MailError>>,
    ) -> Result<T, MailError> {
        match tokio::time::timeout(self.config.timeout, phase_future).await {
            Ok(result) => result,
            Err(_elapsed) => {
                tracing::debug!(phase, timeout = ?self.config.timeout, "SMTP phase timed out");
                Err(TransientFailure::Timeout.into())
            }
        }
    }

    /// Sends one message over one logical send: reuse a pooled connection when possible, drive
    /// the envelope, and only retry on a stale pooled connection that failed before DATA started
    /// (a retry after DATA could duplicate the message).
    async fn send_session(&self, message: &OutgoingMessage) -> Result<SendReceipt, MailError> {
        message.validate()?;
        let message_id = message.resolved_message_id();
        let email = build_mime_with_id(message, &message_id);
        if email.is_empty() {
            return Err(
                PermanentFailure::Client("MIME encoding produced no bytes".to_owned()).into(),
            );
        }
        let mut was_pooled = false;
        let mut connection = match self.take_idle() {
            Some(connection) => {
                was_pooled = true;
                connection
            }
            None => self.connect().await?,
        };
        let mut retried = false;
        loop {
            let mut data_started = false;
            let outcome = self
                .within_timeout(
                    "envelope",
                    self.drive_session(
                        &mut connection,
                        message,
                        &email,
                        &message_id,
                        &mut data_started,
                    ),
                )
                .await;
            match outcome {
                Ok(receipt) => {
                    // The connection stays live for the next send; do not QUIT here.
                    self.release(connection).await;
                    return Ok(receipt);
                }
                Err(error) => {
                    let stale = matches!(
                        error,
                        MailError::Transient(TransientFailure::ConnectionReset)
                    ) && was_pooled
                        && !retried
                        && !data_started;
                    if stale {
                        retried = true;
                        connection.abort().await;
                        connection = self.connect().await?;
                        continue;
                    }
                    connection.abort().await;
                    return Err(error);
                }
            }
        }
    }

    /// Drives one envelope exchange: MAIL, a RCPT per recipient (rejections collected, session
    /// kept alive), then DATA with the MIME bytes; all recipients rejected is an error carrying
    /// the first rejection.
    async fn drive_session(
        &self,
        connection: &mut AsyncSmtpConnection,
        message: &OutgoingMessage,
        email: &[u8],
        message_id: &str,
        data_started: &mut bool,
    ) -> Result<SendReceipt, MailError> {
        let mut mail_parameters: Vec<MailParameter> = Vec::new();
        if !email.is_ascii() {
            if connection
                .server_info()
                .supports_feature(Extension::EightBitMime)
            {
                mail_parameters.push(MailParameter::Body(MailBodyParameter::EightBitMime));
            } else {
                return Err(PermanentFailure::InvalidMessage(
                    "message has non-ASCII content but the server does not advertise 8BITMIME"
                        .to_owned(),
                )
                .into());
            }
        }
        connection
            .command(Mail::new(
                Some(message.from.address.clone()),
                mail_parameters,
            ))
            .await
            .map_err(|e| classify_error(&e, "MAIL"))?;
        let mut accepted: Vec<Address> = Vec::new();
        let mut rejected: Vec<(Address, SmtpReply)> = Vec::new();
        for recipient in message.recipients() {
            match connection
                .command(Rcpt::new(recipient.address.clone(), Vec::new()))
                .await
            {
                Ok(_) => accepted.push(recipient.address.clone()),
                Err(error) => match reply_of(&error) {
                    // A reply keeps the session alive: collect the rejection and continue.
                    Some(reply) => rejected.push((recipient.address.clone(), reply)),
                    // A transport failure may have broken the stream: stop here.
                    None => return Err(classify_error(&error, "RCPT")),
                },
            }
        }
        if accepted.is_empty() {
            if rejected.is_empty() {
                return Err(
                    PermanentFailure::Client("message had no recipients".to_owned()).into(),
                );
            }
            return Err(rejection_error(&rejected));
        }
        connection
            .command(Data)
            .await
            .map_err(|e| classify_error(&e, "DATA"))?;
        *data_started = true;
        connection
            .message(email)
            .await
            .map_err(|e| classify_error(&e, "message data"))?;
        Ok(SendReceipt {
            message_id: message_id.to_owned(),
            accepted,
            rejected,
        })
    }
}

/// The error for "every recipient was rejected": transient when any rejection was a 4xx reply
/// (the queue may retry the whole message later), permanent otherwise, carrying the reply that
/// motivated the classification.
fn rejection_error(rejected: &[(Address, SmtpReply)]) -> MailError {
    let reply = match rejected.iter().find(|(_, reply)| reply.is_transient()) {
        Some((_, reply)) => reply.clone(),
        None => match rejected.first() {
            Some((_, reply)) => reply.clone(),
            None => SmtpReply::new(550, "all recipients rejected"),
        },
    };
    classified_reply(reply)
}

impl MailTransport for SmtpTransport {
    fn send<'a>(
        &'a self,
        message: &'a OutgoingMessage,
    ) -> Pin<Box<dyn Future<Output = Result<SendReceipt, MailError>> + Send + 'a>> {
        Box::pin(self.send_session(message))
    }
}
