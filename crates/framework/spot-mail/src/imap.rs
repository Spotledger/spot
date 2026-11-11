//! The IMAP receive transport (WP-PLT-11.T2): fetches new messages out of one mailbox with
//! [`fetch_new`], normalizes them through `mail-parser`, and classifies failures so the
//! persistent poller (WP-PLT-06) can retry the right ones. Conventions mirror [`crate::smtp`]:
//! one connection per call, per-phase timeouts, redacted credentials, fake servers in tests.
//!
//! One [`fetch_new`] call is one session: connect (implicit TLS or STARTTLS), log in, select
//! the mailbox, search UIDs past the cursor, fetch `RFC822.SIZE`, fetch bodies within
//! [`FetchLimits`] (headers only for oversize messages), and log out. The server state is never
//! modified: bodies are fetched with `BODY.PEEK`, flags are never set, nothing is moved or
//! deleted.
//!
//! Idempotence: a batch is keyed by [`MailboxCursor`]. When the server loses the mailbox
//! (a `uid_validity` change), the batch reports [`FetchOutcome::MailboxReset`] and stops using
//! the stale cursor; the caller decides whether to re-import, deduplicating by `Message-ID`.
//! A dropped connection mid-batch is transient — the cursor only advances past messages
//! actually returned, so the same UIDs are fetched again next poll.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_imap::imap_proto::{AttributeValue, MessageSection, Response, SectionPath, Status};
use async_imap::{Client, Session};
use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::error::{MailError, PermanentFailure, TransientFailure};
use crate::message::Secret;
use crate::smtp::Credentials;

mod normalize;
use normalize::normalize;

/// How the IMAP connection is protected. Plain IMAP is not offered: the credentials travel on
/// the wire and receive servers are reachable from anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapSecurity {
    /// Implicit TLS from the first byte, usually on port 993.
    Tls,
    /// Plain connection upgraded to TLS per STARTTLS, usually on port 143.
    StartTls,
}

/// Static configuration for the IMAP ingestion transport; one instance per polled mailbox.
#[derive(Clone)]
pub struct ImapConfig {
    /// The server host, a name or an address.
    pub host: String,
    /// The server port. `993` for `Tls`, `143` for `StartTls`.
    pub port: u16,
    /// How the connection is protected; plain IMAP is not offered, see [`ImapSecurity`].
    pub security: ImapSecurity,
    /// The LOGIN credentials.
    pub credentials: Credentials,
    /// The mailbox (folder) to poll, for example `INBOX`. Defaults to `INBOX`.
    pub mailbox: String,
    /// The timeout for connecting and for every protocol phase. Defaults to 30 s.
    pub timeout: Duration,
    /// PEM-encoded root certificate(s) that replace the platform certificate store for this
    /// server (private CA setups, test servers). `None` trusts the system root store.
    pub root_certificate_pem: Option<Vec<u8>>,
}

impl Default for ImapConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 993,
            security: ImapSecurity::Tls,
            credentials: Credentials {
                user: String::new(),
                secret: Secret::new(String::new()),
            },
            mailbox: "INBOX".to_owned(),
            timeout: Duration::from_secs(30),
            root_certificate_pem: None,
        }
    }
}

/// The password field must not leak in logs; `Credentials`' own `Debug` redaction covers the
/// secret but the struct has to be wrapped to state that intent for the whole config.
impl fmt::Debug for ImapConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("credentials", &self.credentials)
            .field("mailbox", &self.mailbox)
            .field("timeout", &self.timeout)
            .field(
                "root_certificate_pem",
                &self.root_certificate_pem.as_ref().map(|pem| pem.len()),
            )
            .finish()
    }
}

impl ImapConfig {
    /// Validates the config so an invalid one fails fast, before any connection is attempted.
    pub fn validate(&self) -> Result<(), MailError> {
        let mut invalid: Vec<String> = Vec::new();
        if self.host.is_empty() {
            invalid.push("host must not be empty".to_owned());
        }
        if self.port == 0 {
            invalid.push("port must be non-zero".to_owned());
        }
        if self.mailbox.is_empty() {
            invalid.push("mailbox must not be empty".to_owned());
        }
        if self.timeout.is_zero() {
            invalid.push("timeout must be greater than zero".to_owned());
        }
        if invalid.is_empty() {
            Ok(())
        } else {
            Err(
                PermanentFailure::InvalidMessage(format!("ImapConfig: {}", invalid.join("; ")))
                    .into(),
            )
        }
    }
}

/// The cursor the caller stores between polls: the mailbox generation and the highest UID
/// ingested in that generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxCursor {
    /// The `UIDVALIDITY` the `last_uid` was observed under. Servers bump it when the mailbox is
    /// rebuilt (e.g. after a restore) and UIDs restart; a change invalidates `last_uid`.
    pub uid_validity: u32,
    /// The highest UID ingested under `uid_validity`.
    pub last_uid: u32,
}

/// Cap on one [`fetch_new`] batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchLimits {
    /// At most this many messages are returned per batch; when more remain, the batch says so
    /// with [`FetchBatch::more`]. Defaults to 50.
    pub max_messages: usize,
    /// Messages over this size are returned as header stubs (`truncated: true`); defaults to
    /// 25 MiB.
    pub max_message_bytes: usize,
}

impl Default for FetchLimits {
    fn default() -> Self {
        Self {
            max_messages: 50,
            max_message_bytes: 25 * 1024 * 1024,
        }
    }
}

/// Whether a batch continues the old mailbox generation or the mailbox was rebuilt under the
/// caller's feet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// The cursor's `uid_validity` still matches the mailbox.
    New,
    /// The mailbox's `UIDVALIDITY` differs from the cursor's: UIDs restarted, `last_uid` is
    /// meaningless. The caller decides what to re-import, deduplicating by `Message-ID`.
    MailboxReset,
}

/// One ingestion batch: the normalized messages, the cursor to store afterwards, and whether
/// more messages remain in the mailbox (repeated calls with the returned cursor fetch them).
#[derive(Debug, Clone)]
pub struct FetchBatch {
    /// How the batch relates to the cursor passed in.
    pub outcome: FetchOutcome,
    /// The messages, in ascending UID order.
    pub messages: Vec<IncomingMessage>,
    /// The cursor to store for the next poll.
    pub cursor: MailboxCursor,
    /// More UIDs remain in the mailbox than the limit allowed in this batch.
    pub more: bool,
}

/// One normalized address from a message header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailAddress {
    /// The display name, when present.
    pub name: Option<String>,
    /// The address, lowercased by mail-parser's header normalizer.
    pub address: String,
}

/// One ingested message, normalized out of its MIME structure by [`fetch_new`]. Attachments
/// reuse the crate's outgoing [`crate::message::Attachment`], with `content_id` marking inline
/// parts that match HTML `cid:` references.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    /// The UID inside the mailbox, stable for as long as the batch's `uid_validity` holds.
    pub uid: u32,
    /// The RFC822 size in bytes at fetch time.
    pub raw_size: usize,
    /// True when [`FetchLimits::max_message_bytes` cut the fetch short: headers only, no bodies
    /// and no attachments. Nothing else is dropped silently.
    pub truncated: bool,
    /// The `Message-ID` without angle brackets; the message-level dedupe key.
    pub message_id: Option<String>,
    /// The `In-Reply-To` message id, without angle brackets.
    pub in_reply_to: Option<String>,
    /// The `References` message ids, in wire order, without angle brackets.
    pub references: Vec<String>,
    /// The single from address. Groups are flattened; a multi-value `From:` keeps its first.
    pub from: Option<MailAddress>,
    /// The `To` addresses.
    pub to: Vec<MailAddress>,
    /// The `Cc` addresses.
    pub cc: Vec<MailAddress>,
    /// The `Reply-To` addresses; falls back (implicitly, by absence) when unset.
    pub reply_to: Vec<MailAddress>,
    /// The `Subject`.
    pub subject: Option<String>,
    /// The `Date` header as unix seconds (UTC); absent when missing or unparsable.
    pub date: Option<i64>,
    /// The first `text/plain` body; `None` when the message has none.
    pub text: Option<String>,
    /// The first `text/html` body; `None` when the message has none.
    pub html: Option<String>,
    /// The attachments, including inline parts with their `content_id`.
    pub attachments: Vec<crate::message::Attachment>,
    /// True when the message announces automated origination (`Auto-Submitted` present and not
    /// `no`, or `Precedence: bulk/auto_reply`): auto-replies must never trigger auto-replies.
    pub auto_submitted: bool,
}

/// Derives the conversation key of a message per the brief: the first `References` id (the
/// thread root, what a reply is attached to), else `In-Reply-To`, else `None`. `None` messages
/// start new threads downstream.
pub fn thread_key(message: &IncomingMessage) -> Option<String> {
    message
        .references
        .first()
        .cloned()
        .or_else(|| message.in_reply_to.clone())
}

/// A stream with a per-operation deadline: every single `read`/`write` is bounded, so a silently
/// stuck server cannot hold a phase open even when the phase-level timeout is generous. The
/// deadline resets on every completed operation, which in an IMAP conversation (command, reply,
/// command…) bounds each direction of the exchange.
/// The TCP stream wrapper with per-operation timeouts. Generic over the inner stream so the
/// TLS-upgraded variant keeps its timeouts too.
pub(crate) struct TimeoutStream<S> {
    inner: S,
    timeout: Duration,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> TimeoutStream<S> {
    fn new(inner: S, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            deadline: None,
        }
    }

    fn poll_with_timeout<R>(
        &mut self,
        cx: &mut Context<'_>,
        // Split borrows: the operation polls `inner`, the deadline lives next to it.
        op: impl FnOnce(&mut S, &mut Context<'_>) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        if self.deadline.is_none() {
            self.deadline = Some(Box::pin(tokio::time::sleep(self.timeout)));
        }
        let mut expired = false;
        if let Some(sleep) = self.deadline.as_mut()
            && sleep.as_mut().poll(cx).is_ready()
        {
            expired = true;
        }
        if expired {
            self.deadline = None;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "IMAP I/O timed out",
            )));
        }
        match op(&mut self.inner, cx) {
            Poll::Ready(out) => {
                self.deadline = None;
                Poll::Ready(out)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for TimeoutStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.poll_with_timeout(cx, |inner, cx| Pin::new(inner).poll_read(cx, buf))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for TimeoutStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.poll_with_timeout(cx, |inner, cx| Pin::new(inner).poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.poll_with_timeout(cx, |inner, cx| Pin::new(inner).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.poll_with_timeout(cx, |inner, cx| Pin::new(inner).poll_shutdown(cx))
    }
}

// async-imap requires a Debug impl on the wire type so sessions can trace; the stream contents
// are not interesting (and may contain message bytes).
impl<S> fmt::Debug for TimeoutStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimeoutStream").finish_non_exhaustive()
    }
}

/// The wire between this process and the IMAP server: a TCP stream with per-op timeouts, under
/// TLS from the point where the security mode says so.
pub(crate) type ImapWire = TimeoutStream<tokio::net::TcpStream>;
pub(crate) type ImapTls = tokio_rustls::client::TlsStream<TimeoutStream<tokio::net::TcpStream>>;
pub(crate) type ImapSession = Session<ImapTls>;

/// The TLS connector with its pinned server name, built once per call.
struct TlsSetup {
    connector: tokio_rustls::TlsConnector,
    server_name: ServerName<'static>,
}

/// Builds the TLS context: the config's `root_certificate_pem` replaces the platform store, or
/// the platform store is used as is. Mirrors spot-postgres' conn.rs trust handling.
fn build_tls(config: &ImapConfig) -> Result<TlsSetup, MailError> {
    let mut roots = rustls::RootCertStore::empty();
    match config.root_certificate_pem.as_deref() {
        Some(pem) => {
            for cert in CertificateDer::pem_slice_iter(pem) {
                let cert = cert.map_err(|error| {
                    PermanentFailure::InvalidMessage(format!(
                        "ImapConfig: root_certificate_pem holds an unparsable certificate: {error}"
                    ))
                })?;
                if let Err(error) = roots.add(cert) {
                    tracing::warn!(%error, "ignoring an invalid root_certificate_pem entry");
                }
            }
            if roots.is_empty() {
                return Err(PermanentFailure::InvalidMessage(
                    "ImapConfig: root_certificate_pem holds no usable certificate".to_owned(),
                )
                .into());
            }
        }
        None => {
            let loaded = rustls_native_certs::load_native_certs();
            for error in &loaded.errors {
                tracing::warn!(%error, "could not load a platform root certificate");
            }
            for cert in loaded.certs {
                if let Err(error) = roots.add(cert) {
                    tracing::warn!(%error, "ignoring an invalid platform root certificate");
                }
            }
            if roots.is_empty() {
                return Err(PermanentFailure::Client(
                    "TLS is required but no platform root certificates could be loaded".to_owned(),
                )
                .into());
            }
        }
    }
    let server_name = ServerName::try_from(config.host.clone()).map_err(|error| {
        PermanentFailure::InvalidMessage(format!(
            "ImapConfig: host {host:?} is not a valid TLS server name: {error:?}",
            host = config.host
        ))
    })?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| PermanentFailure::Client(format!("TLS protocol setup failed: {error:?}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsSetup {
        connector: tokio_rustls::TlsConnector::from(Arc::new(client_config)),
        server_name,
    })
}

/// One bounded protocol phase: the whole future must complete within `timeout`, mirroring the
/// per-phase timeouts of [`crate::smtp`]. The wire additionally bounds every single operation.
async fn phase<T>(
    timeout: Duration,
    fut: impl Future<Output = Result<T, MailError>>,
) -> Result<T, MailError> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => {
            tracing::debug!("IMAP phase timed out");
            Err(TransientFailure::Timeout.into())
        }
    }
}

/// Dials the pinned addresses, in order, each bounded by the timeout, and returns the bounded
/// wire of the first that answers, or classifies the last failure. The host name is never
/// resolved: a socket address needs no lookup, and the host serves only as the TLS server name.
async fn dial(config: &ImapConfig, pinned: &[SocketAddr]) -> Result<ImapWire, MailError> {
    let timeout = config.timeout;
    let mut last: Option<MailError> = None;
    for addr in pinned {
        match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(*addr)).await {
            Err(_) => {
                tracing::debug!(port = config.port, "IMAP connect timed out");
                last = Some(TransientFailure::Timeout.into());
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, "IMAP connect failed");
                last = Some(classify_dial(error));
            }
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok(TimeoutStream::new(stream, config.timeout));
            }
        }
    }
    Err(last.unwrap_or_else(no_pinned_addresses))
}

/// The refusal for a missing or empty pinned list. Nothing is dialled and nothing is resolved.
fn no_pinned_addresses() -> MailError {
    PermanentFailure::Client(
        "pinned addresses are required; an IMAP connection never resolves the host by name"
            .to_owned(),
    )
    .into()
}

/// The greeting is the first protocol data; a server that greets with BYE has no room for us.
async fn read_greeting<T: AsyncRead + AsyncWrite + Unpin + fmt::Debug>(
    client: &mut Client<T>,
) -> Result<(), MailError> {
    let greeting = client.read_response().await.map_err(classify_io)?;
    match greeting {
        None => Err(TransientFailure::ConnectionReset.into()),
        Some(response) => match response.parsed() {
            Response::Data {
                status: Status::Bye,
                outcome,
            } => {
                tracing::debug!(
                    information = ?outcome.information,
                    "IMAP server greeted with BYE"
                );
                Err(TransientFailure::ConnectionReset.into())
            }
            _ => Ok(()),
        },
    }
}

/// Wraps a plain wire into TLS, bounding the handshake from both sides. Any handshake failure is
/// transient: the server's certificate chains are outside our control and may recover.
async fn tls_over(
    tls: &TlsSetup,
    stream: ImapWire,
    timeout: Duration,
) -> Result<ImapTls, MailError> {
    let handshake = tls.connector.connect(tls.server_name.clone(), stream);
    match tokio::time::timeout(timeout, handshake).await {
        Ok(Ok(tls_stream)) => Ok(tls_stream),
        Ok(Err(error)) => {
            tracing::debug!(%error, "IMAP TLS handshake failed");
            Err(TransientFailure::TlsHandshake.into())
        }
        Err(_) => Err(TransientFailure::Timeout.into()),
    }
}

/// Establishes the session: dial, TLS by the config's security mode, greeting. Login happens on
/// top of the returned client, which is how async-imap hands out authenticated sessions.
async fn connect(
    config: &ImapConfig,
    pinned: &[SocketAddr],
    tls: &TlsSetup,
) -> Result<ImapSession, MailError> {
    let wire = dial(config, pinned).await?;
    match config.security {
        ImapSecurity::Tls => {
            let tls_stream = tls_over(tls, wire, config.timeout).await?;
            let mut client = Client::new(tls_stream);
            phase(config.timeout, async { read_greeting(&mut client).await }).await?;
            Ok(login(client, config).await?)
        }
        ImapSecurity::StartTls => {
            let mut client = Client::new(wire);
            phase(config.timeout, async { read_greeting(&mut client).await }).await?;
            phase(config.timeout, async {
                client
                    .run_command_and_check_ok("STARTTLS", None)
                    .await
                    .map_err(|error| classify_imap(error, "starttls"))
            })
            .await?;
            let plain = client.into_inner();
            let tls_stream = tls_over(tls, plain, config.timeout).await?;
            let client = Client::new(tls_stream);
            Ok(login(client, config).await?)
        }
    }
}

/// Logs the session in. async-imap's `login` consumes the client and returns it back with the
/// error; the returned connection is unusable for anything but a fresh attempt, and every fetch
/// opens a fresh one anyway, so the client is dropped on failure.
async fn login(client: Client<ImapTls>, config: &ImapConfig) -> Result<ImapSession, MailError> {
    phase(config.timeout, async {
        client
            .login(
                config.credentials.user.as_str(),
                config.credentials.secret.expose(),
            )
            .await
            .map_err(|(error, _client)| classify_imap(error, "login"))
    })
    .await
}

/// Classifies a dial-time I/O error: a refused port is the expected miss; the rest is
/// infrastructure noise a retry may fix, so it is transient too.
fn classify_dial(error: io::Error) -> MailError {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => TransientFailure::ConnectionRefused.into(),
        _ => TransientFailure::ConnectionReset.into(),
    }
}

/// Classifies a wire I/O error; the [`TimeoutStream`] reports its deadlines as `TimedOut`.
fn classify_io(error: io::Error) -> MailError {
    match error.kind() {
        io::ErrorKind::TimedOut => TransientFailure::Timeout.into(),
        _ => TransientFailure::ConnectionReset.into(),
    }
}

/// Classifies an async-imap protocol error for the stage it occurred in. Connection loss and
/// response parse breakage may go away on a fresh connection (transient); a NO on login is a
/// credentials problem and a NO on select a config problem (both permanent); other NOs mean the
/// server could not complete the command now (transient); BAD is a protocol-level rejection.
fn classify_imap(error: async_imap::error::Error, stage: &str) -> MailError {
    match error {
        async_imap::error::Error::Io(inner) => classify_io(inner),
        async_imap::error::Error::ConnectionLost => TransientFailure::ConnectionReset.into(),
        async_imap::error::Error::Parse(parse_error) => {
            tracing::debug!(%parse_error, "IMAP response could not be parsed");
            TransientFailure::ConnectionReset.into()
        }
        async_imap::error::Error::No(text) if stage == "login" => {
            tracing::debug!(%text, "IMAP authentication refused");
            PermanentFailure::Authentication { reply: None }.into()
        }
        async_imap::error::Error::No(text) if stage == "select" => {
            tracing::debug!(%text, "IMAP mailbox rejected");
            PermanentFailure::Client(format!(
                "IMAP rejected the mailbox (does it exist?): {text}"
            ))
            .into()
        }
        async_imap::error::Error::No(text) => {
            tracing::debug!(stage, %text, "IMAP command answered NO");
            TransientFailure::ConnectionReset.into()
        }
        async_imap::error::Error::Bad(text) => MailError::Permanent(PermanentFailure::Client(
            format!("IMAP rejected the command as BAD: {text}"),
        )),
        async_imap::error::Error::Validate(validate_error) => {
            MailError::Permanent(PermanentFailure::InvalidMessage(format!(
                "IMAP command input was invalid: {validate_error}"
            )))
        }
        async_imap::error::Error::Append => MailError::Permanent(PermanentFailure::Client(
            "IMAP append is not used by this client".to_owned(),
        )),
        // The error enum is non-exhaustive; treat unknowns as transient.
        _ => TransientFailure::ConnectionReset.into(),
    }
}

/// Fetches one batch of new messages out of the configured mailbox, dialling only `pinned`.
///
/// `pinned` is what the egress guard authorised for the server (`Authorized::socket_addrs`),
/// and nothing else is dialled: the host name is never resolved and serves only as the TLS
/// server name (certificate verification). An empty list is refused before any socket call.
///
/// `cursor` is the stored [`MailboxCursor`] of the previous batch, if any. The batch's returned
/// cursor is to be stored and passed back; when the connection drops mid-batch the cursor has
/// not advanced past any message that was not fully returned. When the server's `UIDVALIDITY`
/// has changed, the batch reports [`FetchOutcome::MailboxReset`] and stops using the stale
/// `last_uid`; the caller decides whether to re-import, deduplicating by `Message-ID`.
pub async fn fetch_new_pinned(
    config: &ImapConfig,
    pinned: &[SocketAddr],
    cursor: Option<MailboxCursor>,
    limits: FetchLimits,
) -> Result<FetchBatch, MailError> {
    if pinned.is_empty() {
        return Err(no_pinned_addresses());
    }
    config.validate()?;
    let tls = build_tls(config)?;
    tracing::debug!(host = %config.host, port = config.port, mailbox = %config.mailbox, "IMAP fetch starting");
    let mut session = connect(config, pinned, &tls).await?;
    let result = batch(&mut session, config, cursor, limits).await;
    if let Err(error) = session.logout().await {
        tracing::debug!(%error, "IMAP logout failed");
    }
    result
}

/// Fetches one batch for a server named by a **loopback IP literal** (`127.0.0.1`, `::1`), for
/// local development and tests. A literal needs no lookup, so this path never resolves a name;
/// any other host is refused before any socket call, because only addresses an egress guard
/// authorised may be dialled: use [`fetch_new_pinned`].
pub async fn fetch_new(
    config: &ImapConfig,
    cursor: Option<MailboxCursor>,
    limits: FetchLimits,
) -> Result<FetchBatch, MailError> {
    config.validate()?;
    let literal = config
        .host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .ok()
        .filter(IpAddr::is_loopback);
    let Some(ip) = literal else {
        return Err(no_pinned_addresses());
    };
    fetch_new_pinned(config, &[SocketAddr::new(ip, config.port)], cursor, limits).await
}

/// The whole batch over one established session.
async fn batch(
    session: &mut ImapSession,
    config: &ImapConfig,
    cursor: Option<MailboxCursor>,
    limits: FetchLimits,
) -> Result<FetchBatch, MailError> {
    // Selecting the mailbox never marks anything read: bodies go out with BODY.PEEK, and no
    // command in this session sets a flag or moves mail.
    let mailbox = phase(config.timeout, async {
        session
            .select(&config.mailbox)
            .await
            .map_err(|error| classify_imap(error, "select"))
    })
    .await?;
    let uid_validity = mailbox.uid_validity.unwrap_or(0);

    let outcome = match cursor {
        Some(cursor) if cursor.uid_validity != uid_validity => FetchOutcome::MailboxReset,
        _ => FetchOutcome::New,
    };
    let last_uid = match (outcome, cursor) {
        (FetchOutcome::MailboxReset, _) | (FetchOutcome::New, None) => 0,
        (FetchOutcome::New, Some(cursor)) => cursor.last_uid,
    };

    // The `:*` end of the range makes some servers answer with the highest existing UID even
    // when it is below the lower bound (its UID may have been the cursor's), so the results are
    // filtered by UID below regardless.
    let uids = phase(config.timeout, async {
        session
            .uid_search(format!("UID {}:1:*", last_uid + 1))
            .await
            .map_err(|error| classify_imap(error, "uid_search"))
    })
    .await?;
    let mut uids: Vec<u32> = uids
        .into_iter()
        .filter(|&uid| uid > last_uid)
        .collect::<Vec<_>>();
    uids.sort_unstable();

    let more = uids.len() > limits.max_messages;
    uids.truncate(limits.max_messages);
    if uids.is_empty() {
        return Ok(FetchBatch {
            outcome,
            messages: Vec::new(),
            cursor: MailboxCursor {
                uid_validity,
                last_uid,
            },
            more: false,
        });
    }

    // Sizes first: oversize messages only ever get their headers.
    let sizes = fetch_sizes(session, config, &uids).await?;
    let (oversize, normal): (Vec<u32>, Vec<u32>) = uids.iter().copied().partition(|uid| {
        sizes
            .get(uid)
            .is_none_or(|&size| size as usize > limits.max_message_bytes)
    });

    let mut messages = Vec::with_capacity(uids.len());
    let headers_only = fetch_headers(session, config, &oversize).await?;
    let bodies = fetch_bodies(session, config, &normal).await?;
    for uid in &uids {
        if let Some(bytes) = bodies.get(uid) {
            let raw_size = sizes.get(uid).copied().unwrap_or(0) as usize;
            match normalize(bytes, false) {
                Some(message) => messages.push(IncomingMessage {
                    uid: *uid,
                    raw_size,
                    ..message
                }),
                None => {
                    tracing::warn!(uid, "IMAP message body could not be parsed");
                    return Err(TransientFailure::ConnectionReset.into());
                }
            }
        } else if let Some(header_bytes) = headers_only.get(uid) {
            let raw_size = sizes.get(uid).copied().unwrap_or(0) as usize;
            match normalize(header_bytes, true) {
                Some(message) => {
                    // Only the headers were fetched: the caller must keep treating this as a
                    // stub, which is what `truncated` announces.
                    messages.push(IncomingMessage {
                        uid: *uid,
                        raw_size,
                        truncated: true,
                        ..message
                    })
                }
                None => {
                    tracing::warn!(uid, "IMAP message headers could not be parsed");
                    return Err(TransientFailure::ConnectionReset.into());
                }
            }
        } else {
            // The server confirmed the fetch but returned neither a body nor headers; without
            // the bytes nothing can be ingested, and advancing the cursor would lose the mail.
            tracing::warn!(uid, "IMAP fetch confirmed but returned no bytes");
            return Err(TransientFailure::ConnectionReset.into());
        }
    }

    let highest = uids.last().copied().unwrap_or(last_uid);
    Ok(FetchBatch {
        outcome,
        messages,
        cursor: MailboxCursor {
            uid_validity,
            last_uid: last_uid.max(highest),
        },
        more,
    })
}

/// The parts of one fetched message this ingestion actually uses, extracted from a FETCH
/// response's attributes. Anything the server did not send stays `None` and is treated as
/// missing by the callers.
#[derive(Debug, Default)]
struct FetchedParts {
    /// The `RFC822.SIZE` attribute.
    size: Option<u32>,
    /// The `BODY.PEEK[HEADER]` literal bytes.
    header: Option<Vec<u8>>,
    /// The `BODY.PEEK[]` literal bytes, the whole raw message.
    body: Option<Vec<u8>>,
}

impl FetchedParts {
    /// Absorbs one FETCH attribute. `BODY[]` (requested with `PEEK`) is the whole message;
    /// `BODY[HEADER]` is the header block. Other attributes are noise this ingestion ignores.
    fn absorb(&mut self, attribute: &AttributeValue<'_>) {
        match attribute {
            AttributeValue::Rfc822Size(size) => self.size = Some(*size),
            AttributeValue::BodySection { section, data, .. } => match section {
                None => self.body = data.as_deref().map(<[u8]>::to_vec),
                Some(SectionPath::Full(MessageSection::Header)) => {
                    self.header = data.as_deref().map(<[u8]>::to_vec);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// Issues one `UID FETCH` and collects its responses into a map keyed by UID. The responses
/// are read message by message so that an uncompletable batch can never pass for a finished
/// one: the loop returns only on this command's tagged `OK` completion, so a server that dies
/// mid-batch or hangs up without a completion is classified like any other connection loss.
/// The command itself and every response item can fail (both are classified); responses
/// without a usable UID are protocol noise and skipped.
async fn collect_fetch(
    session: &mut ImapSession,
    config: &ImapConfig,
    uids: &[u32],
    query: &str,
) -> Result<HashMap<u32, FetchedParts>, MailError> {
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    let uid_set = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let command = format!("UID FETCH {uid_set} {query}");
    let id = phase(config.timeout, async {
        session
            .run_command(command.as_str())
            .await
            .map_err(|error| classify_imap(error, "uid_fetch"))
    })
    .await?;
    let mut parts: HashMap<u32, FetchedParts> = HashMap::new();
    loop {
        let response = phase(config.timeout, async {
            session.read_response().await.map_err(classify_io)
        })
        .await?;
        let Some(response) = response else {
            // The server closed the connection without a completion in flight. The batch was
            // never confirmed, which for ingestion is the same as having lost it mid-transfer:
            // advancing the cursor here would silently drop mail.
            return Err(TransientFailure::ConnectionReset.into());
        };
        match response.parsed() {
            Response::Fetch(_, attrs) => {
                let mut entry = FetchedParts::default();
                let mut uid = None;
                for attribute in attrs {
                    match attribute {
                        AttributeValue::Uid(value) => uid = Some(*value),
                        _ => entry.absorb(attribute),
                    }
                }
                match uid {
                    Some(uid) if uid > 0 => {
                        parts.insert(uid, entry);
                    }
                    _ => tracing::debug!("IMAP fetch response without UID ignored"),
                }
            }
            Response::Done {
                tag,
                status,
                outcome,
            } if tag == &id => {
                if *status != Status::Ok {
                    tracing::warn!(
                        status = ?status,
                        information = ?outcome.information,
                        "IMAP UID FETCH rejected"
                    );
                    return Err(TransientFailure::ConnectionReset.into());
                }
                return Ok(parts);
            }
            _ => {}
        }
    }
}

/// Fetches `RFC822.SIZE` for each UID (a message whose size the server omits is unusable).
async fn fetch_sizes(
    session: &mut ImapSession,
    config: &ImapConfig,
    uids: &[u32],
) -> Result<HashMap<u32, u32>, MailError> {
    let fetches = collect_fetch(session, config, uids, "(RFC822.SIZE)").await?;
    Ok(fetches
        .into_iter()
        .filter_map(|(uid, parts)| Some((uid, parts.size?)))
        .collect())
}

/// Fetches the full header block (with trailing blank line) of oversize messages.
async fn fetch_headers(
    session: &mut ImapSession,
    config: &ImapConfig,
    uids: &[u32],
) -> Result<HashMap<u32, Vec<u8>>, MailError> {
    let fetches = collect_fetch(session, config, uids, "(BODY.PEEK[HEADER])").await?;
    Ok(fetches
        .into_iter()
        .filter_map(|(uid, parts)| Some((uid, parts.header?)))
        .collect())
}

/// Fetches the complete raw message bytes (headers and body) of normal messages.
async fn fetch_bodies(
    session: &mut ImapSession,
    config: &ImapConfig,
    uids: &[u32],
) -> Result<HashMap<u32, Vec<u8>>, MailError> {
    let fetches = collect_fetch(session, config, uids, "(BODY.PEEK[])").await?;
    Ok(fetches
        .into_iter()
        .filter_map(|(uid, parts)| Some((uid, parts.body?)))
        .collect())
}
