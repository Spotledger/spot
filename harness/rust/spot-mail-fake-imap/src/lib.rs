//! A fake IMAP4rev1 server for other crates' tests (WP-PLT-11.T15). It is a harness crate, not a
//! feature of `spot-mail`: a framework crate offers no `testing` code to a product build.
//!
//! It holds a scripted mailbox, records every session as seen on the wire (LOGIN secrets are
//! redacted) and serves TLS, implicit or through STARTTLS, with a self-signed certificate made at
//! start or an identity the caller supplies. It speaks just enough of the protocol for the
//! ingestion client in `spot_mail::imap`: greeting, CAPABILITY, STARTTLS, LOGIN, SELECT with
//! `UIDVALIDITY` and `UIDNEXT`, UID SEARCH, UID FETCH of `RFC822.SIZE` and `BODY.PEEK` literals,
//! and LOGOUT. It answers no state-changing command.
//!
//! The same server backs `spot-mail`'s own integration tests under `tests/support`, which keep
//! their copy; this crate is the one a harness can depend on. Nothing here panics: a failure to
//! start is a [`TestingError`].

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;

/// The user name the fake server's LOGIN is documented to expect; the secret is never verified.
pub const FAKE_USER: &str = "mailer@spot.test";
/// A password to give the client; the server accepts any (unless the script rejects LOGIN).
pub const FAKE_PASSWORD: &str = "mail-secret";

/// How long the server parks a `no_reply` session before giving up.
const NO_REPLY_PARK: Duration = Duration::from_secs(30);

/// Why a fake server could not start.
#[derive(Debug, thiserror::Error)]
pub enum TestingError {
    /// The listener could not bind or report its address.
    #[error("fake IMAP server socket: {0}")]
    Io(#[from] io::Error),
    /// The certificate or key could not be made, parsed or used.
    #[error("fake IMAP server TLS identity: {0}")]
    Tls(String),
}

/// One message the fake mailbox serves: its server-assigned UID and its raw RFC 5322 bytes.
#[derive(Clone, Debug)]
pub struct FixtureMessage {
    /// The UID this message answers to in SEARCH and FETCH.
    pub uid: u32,
    /// The message bytes; `RFC822.SIZE` is their length.
    pub raw: Vec<u8>,
}

/// How the fake server answers; every switch left at its default behaves like a healthy server.
#[derive(Clone, Debug)]
pub struct ImapScript {
    /// The mailbox content, served in ascending UID order regardless of order here.
    pub messages: Vec<FixtureMessage>,
    /// The `UIDVALIDITY` answered on SELECT.
    pub uid_validity: u32,
    /// Refuse LOGIN with a tagged `NO [AUTHENTICATIONFAILED]`.
    pub reject_login: bool,
    /// Refuse SELECT with a tagged `NO`.
    pub reject_select: bool,
    /// Refuse UID FETCH with a tagged `NO`.
    pub reject_fetch: bool,
    /// Accept the connection but never answer anything; the client times out.
    pub no_reply: bool,
    /// Terminate the connection partway through a UID FETCH, after this many message
    /// responses and before the tagged completion. `0` fetches normally.
    pub drop_after_fetch_responses: usize,
}

impl Default for ImapScript {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            uid_validity: 1_700_000_000,
            reject_login: false,
            reject_select: false,
            reject_fetch: false,
            no_reply: false,
            drop_after_fetch_responses: 0,
        }
    }
}

/// One session as seen on the wire, taken as a snapshot when asked for. The LOGIN secret never
/// appears: the user name is recorded, the argument list is not.
#[derive(Clone, Debug, Default)]
pub struct ImapRecording {
    /// Every command, verbatim with the tag stripped; the LOGIN credentials are redacted.
    pub commands: Vec<String>,
    /// The user name of the (first) LOGIN exchange.
    pub login_user: Option<String>,
    /// Whether the (first) LOGIN exchange was accepted.
    pub login_ok: Option<bool>,
    /// The mailboxes each SELECT opened.
    pub select_mailboxes: Vec<String>,
    /// Each UID SEARCH criteria, as the client spelled it.
    pub uid_searches: Vec<String>,
    /// Each UID FETCH item list, as the client spelled it.
    pub uid_fetches: Vec<String>,
    /// Whether the session ended through LOGOUT.
    pub logout: bool,
    /// Whether the server hung up mid-fetch as scripted.
    pub dropped_mid_fetch: bool,
}

/// A running fake IMAP server; tests drive the ingestion transport against it. The accept loop
/// stops when the handle is dropped.
pub struct ImapFakeServer {
    addr: SocketAddr,
    host_pem: String,
    script: Arc<Mutex<ImapScript>>,
    records: Arc<Mutex<Vec<Arc<Mutex<ImapRecording>>>>>,
    connections: Arc<AtomicUsize>,
    accept: tokio::task::JoinHandle<()>,
}

impl Drop for ImapFakeServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl ImapFakeServer {
    /// The port the client must point at (the server listens on 127.0.0.1).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The certificate PEM the server presents (self-signed, valid for `127.0.0.1` and
    /// `mail.spot.test`, unless an identity was supplied).
    #[must_use]
    pub fn host_certificate_pem(&self) -> &str {
        &self.host_pem
    }

    /// The number of TCP connections accepted so far.
    #[must_use]
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Replaces the script; sessions re-read it at every command boundary, so a change made
    /// between two polls reaches the next poll.
    pub async fn set_script(&self, script: ImapScript) {
        *self.script.lock().await = script;
    }

    /// Adds one message to the mailbox and returns the UID it was given (one above the highest
    /// UID held, starting at 1). `raw` is the complete RFC 5322 message.
    pub async fn push_message(&self, raw: Vec<u8>) -> u32 {
        let mut script = self.script.lock().await;
        let uid = script
            .messages
            .iter()
            .map(|message| message.uid)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        script.messages.push(FixtureMessage { uid, raw });
        uid
    }

    /// The number of messages the mailbox holds.
    pub async fn message_count(&self) -> usize {
        self.script.lock().await.messages.len()
    }

    /// Every session seen so far, in connection order, as snapshots.
    pub async fn recordings(&self) -> Vec<ImapRecording> {
        let handles = self.records.lock().await.clone();
        let mut snapshots = Vec::with_capacity(handles.len());
        for recording in handles {
            snapshots.push(recording.lock().await.clone());
        }
        snapshots
    }

    /// The last session seen, as a snapshot, or `None` before any connection.
    pub async fn last_recording(&self) -> Option<ImapRecording> {
        let last = self.records.lock().await.last().cloned()?;
        let snapshot = last.lock().await.clone();
        Some(snapshot)
    }
}

/// A minimal RFC 5322 message for a test mailbox: `Message-ID`, `From`, `To`, `Subject` and a
/// plain-text body, CRLF line ends. `extra_headers` are `(name, value)` pairs added verbatim
/// (for example `Auto-Submitted`); a name or value holding a line break is dropped, so a caller
/// cannot smuggle a header in through one.
#[must_use]
pub fn plain_message(
    message_id: &str,
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> Vec<u8> {
    let clean = |text: &str| text.replace(['\r', '\n'], " ");
    let mut text = format!(
        "Message-ID: <{}>\r\nFrom: {}\r\nTo: {}\r\nSubject: {}\r\nDate: Sat, 14 Nov 2026 09:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n",
        clean(message_id),
        clean(from),
        clean(to),
        clean(subject)
    );
    for (name, value) in extra_headers {
        if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
            continue;
        }
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    text.push_str(body);
    text.push_str("\r\n");
    text.into_bytes()
}

/// Spawns a fake server that negotiates TLS from the first byte (implicit TLS on the port),
/// with a fresh self-signed certificate.
pub async fn spawn_tls() -> Result<ImapFakeServer, TestingError> {
    let (acceptor, pem) = self_signed_acceptor()?;
    spawn(false, acceptor, pem).await
}

/// Spawns a fake server whose greeting and protocol start plain and upgrade via STARTTLS, with
/// a fresh self-signed certificate.
pub async fn spawn_starttls() -> Result<ImapFakeServer, TestingError> {
    let (acceptor, pem) = self_signed_acceptor()?;
    spawn(true, acceptor, pem).await
}

/// Spawns an implicit-TLS fake server presenting the given PEM certificate and PKCS#8 / PKCS#1 /
/// SEC1 PEM private key, for a harness whose process already trusts that certificate.
pub async fn spawn_tls_with_identity(
    certificate_pem: &str,
    private_key_pem: &str,
) -> Result<ImapFakeServer, TestingError> {
    let certificates = CertificateDer::pem_slice_iter(certificate_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TestingError::Tls(format!("certificate PEM: {error}")))?;
    if certificates.is_empty() {
        return Err(TestingError::Tls(
            "the certificate PEM holds no certificate".to_owned(),
        ));
    }
    let key = PrivateKeyDer::from_pem_slice(private_key_pem.as_bytes())
        .map_err(|error| TestingError::Tls(format!("private key PEM: {error}")))?;
    let acceptor = acceptor_from(certificates, key)?;
    spawn(false, acceptor, certificate_pem.to_owned()).await
}

fn self_signed_acceptor() -> Result<(TlsAcceptor, String), TestingError> {
    let certified = rcgen::generate_simple_self_signed(vec![
        "127.0.0.1".to_owned(),
        "mail.spot.test".to_owned(),
    ])
    .map_err(|error| TestingError::Tls(error.to_string()))?;
    let pem = certified.cert.pem();
    let certificate = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    Ok((acceptor_from(vec![certificate], key)?, pem))
}

fn acceptor_from(
    certificates: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<TlsAcceptor, TestingError> {
    // The provider is named explicitly: rustls panics when none is chosen and several exist.
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| TestingError::Tls(error.to_string()))?
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .map_err(|error| TestingError::Tls(error.to_string()))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn spawn(
    starttls: bool,
    acceptor: TlsAcceptor,
    host_pem: String,
) -> Result<ImapFakeServer, TestingError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let state = ImapState {
        script: Arc::new(Mutex::new(ImapScript::default())),
        records: Arc::new(Mutex::new(Vec::new())),
        connections: Arc::new(AtomicUsize::new(0)),
    };
    let accept = tokio::spawn(serve(listener, acceptor, starttls, state.clone()));
    Ok(ImapFakeServer {
        addr,
        host_pem,
        script: state.script,
        records: state.records,
        connections: state.connections,
        accept,
    })
}

#[derive(Clone)]
struct ImapState {
    script: Arc<Mutex<ImapScript>>,
    records: Arc<Mutex<Vec<Arc<Mutex<ImapRecording>>>>>,
    connections: Arc<AtomicUsize>,
}

async fn serve(listener: TcpListener, acceptor: TlsAcceptor, starttls: bool, state: ImapState) {
    loop {
        let Ok((stream, _peer)) = listener.accept().await else {
            return;
        };
        state.connections.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(imap_session(
            stream,
            acceptor.clone(),
            starttls,
            state.clone(),
        ));
    }
}

/// A client connection that either is plain TCP or has been upgraded to TLS.
enum Wire {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl AsyncRead for Wire {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Reads one command line; `None` when the peer is gone.
async fn read_command(wire: &mut Wire) -> Option<String> {
    let mut reader = BufReader::new(&mut *wire);
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

/// Writes bytes; true when the write failed and the session must close.
async fn write_bytes(wire: &mut Wire, bytes: &[u8]) -> bool {
    wire.write_all(bytes).await.is_err()
}

/// Writes a reply; true when the write failed and the session must close.
async fn write_or_close(wire: &mut Wire, reply: &str) -> bool {
    write_bytes(wire, reply.as_bytes()).await
}

/// One protocol phase ends in a hangup or, after STARTTLS, in a connection upgrade.
enum ImapPhase {
    Closed,
    Upgraded(Wire),
}

async fn imap_session(stream: TcpStream, acceptor: TlsAcceptor, starttls: bool, state: ImapState) {
    let mut wire = if starttls {
        Wire::Plain(stream)
    } else {
        match acceptor.accept(stream).await {
            Ok(tls) => Wire::Tls(Box::new(tls)),
            Err(_) => return,
        }
    };
    let recording = Arc::new(Mutex::new(ImapRecording::default()));
    state.records.lock().await.push(recording.clone());
    let mut greeted = false;
    loop {
        match imap_phase(wire, &acceptor, &state, &recording, !greeted).await {
            ImapPhase::Closed => break,
            ImapPhase::Upgraded(next) => {
                greeted = true;
                wire = next;
            }
        }
    }
}

async fn imap_phase(
    mut wire: Wire,
    acceptor: &TlsAcceptor,
    state: &ImapState,
    recording: &Arc<Mutex<ImapRecording>>,
    greet: bool,
) -> ImapPhase {
    if greet {
        let script = state.script.lock().await.clone();
        if script.no_reply {
            tokio::time::sleep(NO_REPLY_PARK).await;
            return ImapPhase::Closed;
        }
        let greeting = "* OK [CAPABILITY IMAP4rev1 STARTTLS] spot.test ready\r\n";
        if write_or_close(&mut wire, greeting).await {
            return ImapPhase::Closed;
        }
    }
    loop {
        let Some(line) = read_command(&mut wire).await else {
            return ImapPhase::Closed;
        };
        let Some((tag, command)) = line.trim_end().split_once(' ') else {
            let _ = write_or_close(&mut wire, "* BAD missing tag\r\n").await;
            continue;
        };
        // The script is re-read at every command boundary so a `set_script` or `push_message`
        // between two polls takes effect on the next one.
        let script = state.script.lock().await.clone();
        let mut recording = recording.lock().await;
        let (verb, arguments) = match command.split_once(' ') {
            Some((verb, rest)) => (verb.to_ascii_uppercase(), rest.trim().to_owned()),
            None => (command.to_ascii_uppercase(), String::new()),
        };
        if verb == "LOGIN" {
            recording.commands.push("LOGIN [secret]".to_owned());
            recording.login_user = arguments
                .split_whitespace()
                .next()
                .map(|user| user.trim_matches('"').to_owned());
            recording.login_ok = Some(!script.reject_login);
            let reply = if script.reject_login {
                format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n")
            } else {
                format!("{tag} OK [CAPABILITY IMAP4rev1] Logged in\r\n")
            };
            if write_or_close(&mut wire, &reply).await {
                return ImapPhase::Closed;
            }
            continue;
        }
        recording.commands.push(format!("{verb} {arguments}"));
        match verb.as_str() {
            "CAPABILITY" => {
                let reply =
                    format!("* CAPABILITY IMAP4rev1 STARTTLS\r\n{tag} OK CAPABILITY completed\r\n");
                if write_or_close(&mut wire, &reply).await {
                    return ImapPhase::Closed;
                }
            }
            "STARTTLS" => {
                let reply = format!("{tag} OK Begin TLS negotiation now\r\n");
                if write_or_close(&mut wire, &reply).await {
                    return ImapPhase::Closed;
                }
                let Wire::Plain(stream) = wire else {
                    return ImapPhase::Closed;
                };
                return match acceptor.accept(stream).await {
                    Ok(tls) => ImapPhase::Upgraded(Wire::Tls(Box::new(tls))),
                    Err(_) => ImapPhase::Closed,
                };
            }
            "SELECT" => {
                recording
                    .select_mailboxes
                    .push(arguments.trim_matches('"').to_owned());
                let reply = if script.reject_select {
                    format!("{tag} NO no such mailbox\r\n")
                } else {
                    select_reply(&script, tag)
                };
                if write_or_close(&mut wire, &reply).await {
                    return ImapPhase::Closed;
                }
            }
            "LOGOUT" => {
                recording.logout = true;
                let _ = write_or_close(&mut wire, "* BYE spot.test closing connection\r\n").await;
                let _ = write_or_close(&mut wire, &format!("{tag} OK LOGOUT completed\r\n")).await;
                return ImapPhase::Closed;
            }
            "UID" => {
                let Some((subcommand, rest)) = arguments.split_once(' ') else {
                    let reply = format!("{tag} BAD UID needs a subcommand\r\n");
                    if write_or_close(&mut wire, &reply).await {
                        return ImapPhase::Closed;
                    }
                    continue;
                };
                match subcommand.to_ascii_uppercase().as_str() {
                    "SEARCH" => {
                        recording.uid_searches.push(rest.to_owned());
                        let reply = search_reply(&script, tag, rest);
                        if write_or_close(&mut wire, &reply).await {
                            return ImapPhase::Closed;
                        }
                    }
                    "FETCH" => {
                        let Some((uid_set, items)) = rest.split_once(' ') else {
                            let reply = format!("{tag} BAD FETCH needs messages and items\r\n");
                            if write_or_close(&mut wire, &reply).await {
                                return ImapPhase::Closed;
                            }
                            continue;
                        };
                        recording.uid_fetches.push(items.to_owned());
                        if script.reject_fetch {
                            let reply = format!("{tag} NO fetch failed\r\n");
                            if write_or_close(&mut wire, &reply).await {
                                return ImapPhase::Closed;
                            }
                            continue;
                        }
                        match fetch(&mut wire, &script, tag, uid_set, items).await {
                            FetchEnd::Done => {}
                            FetchEnd::Dropped => {
                                recording.dropped_mid_fetch = true;
                                return ImapPhase::Closed;
                            }
                            FetchEnd::WriteFailed => return ImapPhase::Closed,
                        }
                    }
                    _ => {
                        let reply = format!("{tag} BAD unknown UID subcommand\r\n");
                        if write_or_close(&mut wire, &reply).await {
                            return ImapPhase::Closed;
                        }
                    }
                }
            }
            _ => {
                let reply = format!("{tag} BAD unknown command\r\n");
                if write_or_close(&mut wire, &reply).await {
                    return ImapPhase::Closed;
                }
            }
        }
    }
}

/// The untagged SELECT answers and the tagged completion.
fn select_reply(script: &ImapScript, tag: &str) -> String {
    let uid_next = script
        .messages
        .iter()
        .map(|message| message.uid)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    format!(
        "* {} EXISTS\r\n* OK [UIDVALIDITY {}] UIDs valid\r\n* OK [UIDNEXT {uid_next}] Predicted next UID\r\n* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n{tag} OK [READ-WRITE] SELECT completed\r\n",
        script.messages.len(),
        script.uid_validity
    )
}

/// How a UID FETCH ended.
enum FetchEnd {
    Done,
    Dropped,
    WriteFailed,
}

/// Answers a UID FETCH for the wanted UIDs in ascending order.
async fn fetch(
    wire: &mut Wire,
    script: &ImapScript,
    tag: &str,
    uid_set: &str,
    items: &str,
) -> FetchEnd {
    let wanted = uid_set
        .split(',')
        .filter_map(|uid| uid.parse::<u32>().ok())
        .collect::<std::collections::HashSet<_>>();
    let mut sorted = script.messages.clone();
    sorted.sort_by_key(|message| message.uid);
    let mut answered = 0usize;
    for message in sorted
        .iter()
        .filter(|message| wanted.contains(&message.uid))
    {
        answered += 1;
        let mut part = format!("* {answered} FETCH (UID {}", message.uid);
        if items.contains("RFC822.SIZE") {
            part.push_str(&format!(" RFC822.SIZE {}", message.raw.len()));
        }
        // The server answers BODY[...] even when the client asked to peek; which section the
        // client requested is what tests assert on the recording.
        let literal: &[u8] = if items.contains("BODY.PEEK[HEADER]") {
            let block = header_block(&message.raw);
            part.push_str(&format!(" BODY[HEADER] {{{}}}", block.len()));
            block
        } else if items.contains("BODY.PEEK[]") {
            part.push_str(&format!(" BODY[] {{{}}}", message.raw.len()));
            &message.raw
        } else {
            &[]
        };
        let mut bytes = part.into_bytes();
        if literal.is_empty() {
            bytes.extend_from_slice(b")\r\n");
        } else {
            bytes.extend_from_slice(b"\r\n");
            bytes.extend_from_slice(literal);
            bytes.extend_from_slice(b")\r\n");
        }
        if write_bytes(wire, &bytes).await {
            return FetchEnd::WriteFailed;
        }
        if script.drop_after_fetch_responses > 0 && answered >= script.drop_after_fetch_responses {
            return FetchEnd::Dropped;
        }
    }
    if write_or_close(wire, &format!("{tag} OK FETCH completed\r\n")).await {
        return FetchEnd::WriteFailed;
    }
    FetchEnd::Done
}

/// The untagged SEARCH answer plus the tagged completion for the criteria the client sent.
fn search_reply(script: &ImapScript, tag: &str, criteria: &str) -> String {
    // `UID {n}:*` is the only criteria this client issues; every UID at or above `n` answers.
    let lower = criteria
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|bound| bound.split(':').next())
        .and_then(|bound| bound.parse::<u32>().ok())
        .unwrap_or(0);
    let mut uids = script
        .messages
        .iter()
        .map(|message| message.uid)
        .filter(|uid| *uid >= lower)
        .collect::<Vec<_>>();
    uids.sort_unstable();
    if uids.is_empty() {
        return format!("* SEARCH\r\n{tag} OK SEARCH completed\r\n");
    }
    let list = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    format!("* SEARCH {list}\r\n{tag} OK SEARCH completed\r\n")
}

/// Everything before the blank line, blank line excluded.
fn header_block(raw: &[u8]) -> &[u8] {
    let end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or(raw.len());
    &raw[..end]
}
