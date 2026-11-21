#![allow(dead_code)]
// Every integration-test binary compiles this whole module, but each test binary drives only the
// server it needs; the unused half of the shared harness trips the dead_code lint per binary.

//! Test harness shared by the spot-mail integration tests: a fake ESMTP server the lettre-based
//! sender runs against (accepts scripted replies at each protocol stage, records every session as
//! seen on the wire, and can serve TLS — implicit or via STARTTLS — with a self-signed certificate
//! generated at startup; it speaks just enough of the protocol: greeting, EHLO with
//! AUTH/PIPELINING/8BITMIME/STARTTLS, AUTH PLAIN and LOGIN, MAIL/RCPT/DATA with dot-stuffing,
//! RSET/NOOP/QUIT), a fake IMAP4rev1 server for the ingest side (`imap_server`), and the
//! test-only error expectations (`Must`/`MustErr`) both rely on.

pub mod imap_server;

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::{TcpListener, TcpStream};

/// Test-only unwrapping that names the failure. The workspace denies `unwrap_used` and, via
/// `-D warnings`, treats `expect_used` (warn) as an error, so tests go through `must()` (and
/// `must_err()` where the error value is asserted on) instead. Mirrors the in-crate `Must`
/// trait, which is `pub(crate)` + `#[cfg(test)]` and therefore invisible from this separate
/// integration-test binary.
pub(crate) trait Must<T> {
    fn must(self) -> T;
}

impl<T, E> Must<T> for Result<T, E>
where
    E: std::fmt::Debug,
{
    fn must(self) -> T {
        self.unwrap_or_else(|error| panic!("unexpected error: {error:?}"))
    }
}

impl<T> Must<T> for Option<T> {
    fn must(self) -> T {
        self.unwrap_or_else(|| panic!("unexpected None"))
    }
}

/// Test-only expectation of an error, in the spirit of [`Must`]: panics when the call returns
/// `Ok`, so a test that needs the error value can still name what would have broken.
pub(crate) trait MustErr<T, E> {
    fn must_err(self) -> E;
}

impl<T, E> MustErr<T, E> for Result<T, E>
where
    E: std::fmt::Debug,
{
    fn must_err(self) -> E {
        match self {
            Ok(_) => panic!("unexpected success"),
            Err(error) => error,
        }
    }
}
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;

/// The credentials the fake server is happy to accept (it does not verify the secret).
pub const FAKE_USER: &str = "mailer@spot.test";
pub const FAKE_PASSWORD: &str = "mail-secret";

/// How long the fake server parks a `no_reply` session before giving up (the client-side
/// timeout of these tests fires long before).
const NO_REPLY_PARK: Duration = Duration::from_secs(30);

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

/// A single scripted reply line for one command.
#[derive(Clone, Debug)]
pub struct Reply {
    /// The SMTP reply code, for example `535`.
    pub code: u16,
    /// The reply text after the code.
    pub text: String,
}

impl Reply {
    /// Builds one reply.
    pub fn new(code: u16, text: impl Into<String>) -> Self {
        Self {
            code,
            text: text.into(),
        }
    }

    fn wire(&self) -> String {
        format!("{} {}\r\n", self.code, self.text)
    }
}

/// How the fake server answers; every stage not mentioned succeeds.
#[derive(Clone, Debug, Default)]
pub struct Script {
    /// Refuse authentication for every session with `535`.
    pub reject_auth: bool,
    /// Reject RCPT for these addresses with these replies.
    pub reject_rcpt: Vec<(String, Reply)>,
    /// Override the reply given to MAIL FROM (the envelope stops at that stage).
    pub mail_reply: Option<Reply>,
    /// Override the reply given to DATA (the message body is never asked for).
    pub data_reply: Option<Reply>,
    /// Accept the connection but never answer anything: the client times out.
    pub no_reply: bool,
    /// Answer MAIL FROM with `250` and then hang up mid-session: simulates a server restart
    /// that kills an idle pooled connection just before the client reuses it.
    pub hangup_after_mail: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Recording {
    /// The commands in order; AUTH lines carry only the mechanism, credentials are redacted.
    pub commands: Vec<String>,
    /// The envelope sender, when a MAIL FROM was accepted.
    pub mail_from: Option<String>,
    /// The RCPT TO addresses that were accepted, in order.
    pub accepted_rcpt: Vec<String>,
    /// RCPT rejections: (address, reply code), in order.
    pub rejected_rcpt: Vec<(String, u16)>,
    /// Whether an authentication exchange ended accepted.
    pub authenticated: bool,
    /// The DATA body after dot-unescape, when one was received.
    pub body: Option<String>,
}

/// A running fake server; tests drive the client-side transport against it.
pub struct FakeServer {
    addr: SocketAddr,
    host_pem: Option<String>,
    script: Arc<Mutex<Script>>,
    records: Arc<Mutex<Vec<Arc<Mutex<Recording>>>>>,
    connections: Arc<AtomicUsize>,
}

impl FakeServer {
    /// The port the transport must point at.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The self-signed certificate PEM, when this server speaks TLS.
    pub fn host_certificate_pem(&self) -> Option<&str> {
        self.host_pem.as_deref()
    }

    /// The number of TCP connections accepted so far (how the pool-reuse tests count).
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Replaces the script; every session re-reads it at each command boundary, so changes
    /// reach a live pooled session too.
    pub async fn set_script(&self, script: Script) {
        *self.script.lock().await = script;
    }

    /// Every session seen so far — registered when its connection was accepted — in connection
    /// order, taking them out. The returned recordings are snapshots.
    pub async fn take_recordings(&self) -> Vec<Recording> {
        let slots = std::mem::take(&mut *self.records.lock().await);
        let mut recordings = Vec::with_capacity(slots.len());
        for slot in slots {
            recordings.push(slot.lock().await.clone());
        }
        recordings
    }

    /// The last session seen, as a snapshot.
    pub async fn last_recording(&self) -> Recording {
        let slot = self.records.lock().await.last().cloned().must();
        slot.lock().await.clone()
    }
}

/// The TLS mode the fake server speaks.
#[derive(Clone, Copy)]
enum Mode {
    Plain,
    StartTls,
    Tls,
}

/// Spawns the default plain fake server (accepts everything, no TLS).
pub async fn spawn_plain() -> FakeServer {
    spawn(Mode::Plain).await
}

/// Spawns a fake server that advertises STARTTLS and upgrades on demand.
pub async fn spawn_starttls() -> FakeServer {
    spawn(Mode::StartTls).await
}

/// Spawns a fake server that speaks TLS from the first byte.
pub async fn spawn_tls() -> FakeServer {
    spawn(Mode::Tls).await
}

/// Builds a TLS acceptor over a fresh self-signed certificate for 127.0.0.1, returning its PEM.
fn acceptor() -> (TlsAcceptor, String) {
    let certified = rcgen::generate_simple_self_signed(vec![
        "127.0.0.1".to_owned(),
        "mail.spot.test".to_owned(),
    ])
    .must();
    let pem = certified.cert.pem();
    let certificate = rustls::pki_types::CertificateDer::from(certified.cert.der().to_vec());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .must();
    (TlsAcceptor::from(Arc::new(config)), pem)
}

async fn spawn(mode: Mode) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.must();
    let addr = listener.local_addr().must();
    let (acceptor, host_pem, starttls) = match mode {
        Mode::Plain => (None, None, false),
        Mode::StartTls => {
            let (acceptor, host_pem) = acceptor();
            (Some(acceptor), Some(host_pem), true)
        }
        Mode::Tls => {
            let (acceptor, host_pem) = acceptor();
            (Some(acceptor), Some(host_pem), false)
        }
    };
    let state = ServerState {
        script: Arc::new(tokio::sync::Mutex::new(Script::default())),
        records: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        connections: Arc::new(AtomicUsize::new(0)),
    };
    let task_state = ServerState {
        script: state.script.clone(),
        records: state.records.clone(),
        connections: state.connections.clone(),
    };
    tokio::spawn(serve(listener, acceptor, starttls, task_state));
    FakeServer {
        addr,
        host_pem,
        script: state.script,
        records: state.records,
        connections: state.connections,
    }
}

/// State shared by the accept loop and the test-facing handle.
#[derive(Clone)]
struct ServerState {
    script: Arc<Mutex<Script>>,
    records: Arc<Mutex<Vec<Arc<Mutex<Recording>>>>>,
    connections: Arc<AtomicUsize>,
}

/// Accepts client connections until the server handle is dropped.
async fn serve(
    listener: TcpListener,
    acceptor: Option<TlsAcceptor>,
    starttls: bool,
    state: ServerState,
) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => continue,
        };
        state.connections.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(session(stream, acceptor.clone(), starttls, state.clone()));
    }
}

/// End of one protocol phase: the session closed, or the wire was replaced (TLS upgrade).
enum PhaseResult {
    Closed,
    Upgraded(Wire),
}

/// Serves one connection to completion and records the session. TLS happens either up front
/// (implicit) or at the STARTTLS command; both share one session recording.
/// Registers the session recording before any command is served, so a pooled session (which
/// stays open until the transport is dropped) is visible to tests while it lives. Sessions on
/// one server are strictly sequential in these tests, which is why it is safe to hold the
/// recording slot for the whole session.
async fn session(
    stream: TcpStream,
    acceptor: Option<TlsAcceptor>,
    starttls: bool,
    state: ServerState,
) {
    let mut wire = match (&acceptor, starttls) {
        (Some(acceptor), false) => match acceptor.accept(stream).await {
            Ok(tls) => Wire::Tls(Box::new(tls)),
            Err(_) => return,
        },
        _ => Wire::Plain(stream),
    };
    // Registered at accept so a pooled session (which stays open until the transport is
    // dropped) is visible to tests while it lives.
    state
        .records
        .lock()
        .await
        .push(Arc::new(Mutex::new(Recording::default())));
    let recording = state.records.lock().await.last().cloned().must();
    let mut greeted = false;
    loop {
        match phase(wire, acceptor.as_ref(), &state, &recording, !greeted).await {
            PhaseResult::Closed => break,
            // The upgrade keeps one connection: the next phase continues without a greeting.
            PhaseResult::Upgraded(next) => {
                greeted = true;
                wire = next;
            }
        }
    }
}

/// Serves commands until QUIT, an unrecoverable read, or a STARTTLS upgrade, which hands the
/// upgraded wire back so the caller can continue the same session over TLS. The server answers
/// each command before the next one (it never advertises PIPELINING), so a fresh buffered read
/// per command cannot lose anything a client sent ahead.
///
/// `greet` sends the initial greeting line: exactly once per connection — a STARTTLS upgrade
/// must not re-greet, or the client's post-upgrade EHLO would read the greeting as its reply
/// and see no advertised capabilities.
async fn phase(
    wire: Wire,
    acceptor: Option<&TlsAcceptor>,
    state: &ServerState,
    recording: &Arc<Mutex<Recording>>,
    greet: bool,
) -> PhaseResult {
    let mut wire = wire;
    let script = state.script.lock().await.clone();
    if greet {
        if script.no_reply {
            // Hold the connection open without answering anything; the client times out first.
            tokio::time::sleep(NO_REPLY_PARK).await;
            return PhaseResult::Closed;
        }
        if write_or_close(&mut wire, "220 spot.test fake ESMTP ready\r\n").await {
            return PhaseResult::Closed;
        }
    }
    loop {
        let line = match read_command(&mut wire).await {
            Some(line) => line.trim_end().to_owned(),
            None => return PhaseResult::Closed,
        };
        // The script is re-read at every command boundary so that a `set_script` issued while a
        // pooled connection sits idle between two sends takes effect mid-session.
        let script = state.script.lock().await.clone();
        // Mutations land immediately in the shared slot; the lock is per command, never held
        // across phases, so tests can snapshot a recording while the session still lives.
        let mut recording = recording.lock().await;
        let (verb, args) = split_command(&line);
        match verb.as_str() {
            "EHLO" | "HELO" => {
                recording.commands.push(line);
                let mut reply = String::from("250-spot.test greets you\r\n250-8BITMIME\r\n");
                if acceptor.is_some() {
                    reply.push_str("250-STARTTLS\r\n");
                }
                reply.push_str("250-AUTH PLAIN LOGIN\r\n250 HELP\r\n");
                if write_or_close(&mut wire, &reply).await {
                    return PhaseResult::Closed;
                }
            }
            "AUTH" => {
                recording.commands.push(redact_auth(&line));
                if script.reject_auth {
                    if write_or_close(
                        &mut wire,
                        "535 5.7.8 authentication credentials rejected\r\n",
                    )
                    .await
                    {
                        return PhaseResult::Closed;
                    }
                } else if args.to_ascii_uppercase().starts_with("PLAIN") {
                    recording.authenticated = true;
                    if write_or_close(&mut wire, "235 2.7.0 Accepted\r\n").await {
                        return PhaseResult::Closed;
                    }
                } else if args.to_ascii_uppercase().starts_with("LOGIN") {
                    if write_or_close(&mut wire, "334 VXNlcm5hbWU6\r\n").await {
                        return PhaseResult::Closed;
                    }
                    if read_command(&mut wire).await.is_none() {
                        return PhaseResult::Closed;
                    }
                    recording.commands.push("[login user, redacted]".to_owned());
                    if write_or_close(&mut wire, "334 UGFzc3dvcmQ6\r\n").await {
                        return PhaseResult::Closed;
                    }
                    if read_command(&mut wire).await.is_none() {
                        return PhaseResult::Closed;
                    }
                    recording
                        .commands
                        .push("[login password, redacted]".to_owned());
                    recording.authenticated = true;
                    if write_or_close(&mut wire, "235 2.7.0 Accepted\r\n").await {
                        return PhaseResult::Closed;
                    }
                } else if write_or_close(&mut wire, "504 5.5.4 unrecognized mechanism\r\n").await {
                    return PhaseResult::Closed;
                }
            }
            "MAIL" => {
                recording.commands.push(line.clone());
                if let Some(reply) = &script.mail_reply {
                    if write_or_close(&mut wire, &reply.wire()).await {
                        return PhaseResult::Closed;
                    }
                } else {
                    recording.mail_from = angle_arg(args);
                    if write_or_close(&mut wire, "250 2.1.0 Ok\r\n").await {
                        return PhaseResult::Closed;
                    }
                    if script.hangup_after_mail {
                        // Drop the socket after the accepted MAIL, like a restarting server. The
                        // restart is a one-off: it deactivates itself so the retried envelope
                        // completes like it does against a restarted-and-recovered server.
                        *state.script.lock().await = Script::default();
                        return PhaseResult::Closed;
                    }
                }
            }
            "RCPT" => {
                recording.commands.push(line.clone());
                let address = angle_arg(args).unwrap_or_default();
                if let Some((_, reply)) = script
                    .reject_rcpt
                    .iter()
                    .find(|(bad, _)| bad.eq_ignore_ascii_case(&address))
                {
                    recording.rejected_rcpt.push((address, reply.code));
                    if write_or_close(&mut wire, &reply.wire()).await {
                        return PhaseResult::Closed;
                    }
                } else {
                    recording.accepted_rcpt.push(address);
                    if write_or_close(&mut wire, "250 2.1.5 Ok\r\n").await {
                        return PhaseResult::Closed;
                    }
                }
            }
            "DATA" => {
                recording.commands.push(line);
                if let Some(reply) = &script.data_reply {
                    if write_or_close(&mut wire, &reply.wire()).await {
                        return PhaseResult::Closed;
                    }
                } else {
                    if write_or_close(&mut wire, "354 End data with <CR><LF>.<CR><LF>\r\n").await {
                        return PhaseResult::Closed;
                    }
                    // One buffered reader drains the whole body to the terminator: the client
                    // pushes the body in few large writes, so reads here must not lose what an
                    // earlier call already buffered.
                    let mut reader = BufReader::new(&mut wire);
                    let mut body = String::new();
                    let terminated = loop {
                        let mut raw = String::new();
                        match reader.read_line(&mut raw).await {
                            Ok(0) | Err(_) => break false,
                            Ok(_) => {}
                        }
                        if raw.trim_end() == "." {
                            break true;
                        }
                        body.push_str(&unescape_dots(&raw));
                    };
                    if !terminated {
                        return PhaseResult::Closed;
                    }
                    recording.body = Some(body);
                    if write_or_close(&mut wire, "250 2.0.0 Ok: queued as spot-1\r\n").await {
                        return PhaseResult::Closed;
                    }
                }
            }
            "RSET" => {
                recording.commands.push(line);
                recording.mail_from = None;
                recording.accepted_rcpt.clear();
                recording.rejected_rcpt.clear();
                recording.body = None;
                if write_or_close(&mut wire, "250 2.0.0 Ok\r\n").await {
                    return PhaseResult::Closed;
                }
            }
            "NOOP" => {
                recording.commands.push(line);
                if write_or_close(&mut wire, "250 2.0.0 Ok\r\n").await {
                    return PhaseResult::Closed;
                }
            }
            "QUIT" => {
                recording.commands.push(line);
                if write_or_close(&mut wire, "221 2.0.0 spot.test closing connection\r\n").await {
                    return PhaseResult::Closed;
                }
                return PhaseResult::Closed;
            }
            "STARTTLS" => {
                recording.commands.push(line);
                let acceptor = match acceptor {
                    Some(acceptor) => acceptor,
                    None => {
                        if write_or_close(&mut wire, "454 4.7.0 TLS not available\r\n").await {
                            return PhaseResult::Closed;
                        }
                        continue;
                    }
                };
                if write_or_close(&mut wire, "220 Ready to start TLS\r\n").await {
                    return PhaseResult::Closed;
                }
                // The upgrade is the last command of the phase: destructure the owned wire.
                let stream = match wire {
                    Wire::Plain(stream) => stream,
                    Wire::Tls(_) => return PhaseResult::Closed,
                };
                let tls = match acceptor.accept(stream).await {
                    Ok(tls) => tls,
                    Err(error) => {
                        eprintln!("server: STARTTLS accept failed: {error}");
                        return PhaseResult::Closed;
                    }
                };
                return PhaseResult::Upgraded(Wire::Tls(Box::new(tls)));
            }
            _ => {
                if write_or_close(&mut wire, "500 5.5.2 unrecognized command\r\n").await {
                    return PhaseResult::Closed;
                }
            }
        }
    }
}

/// Reads one CRLF-terminated command from the client, or `None` at the end of the stream. Safe
/// only between replies (the server always replies before the next command arrives).
async fn read_command(wire: &mut Wire) -> Option<String> {
    let mut reader = BufReader::new(&mut *wire);
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

/// Writes a reply; true when the write failed and the session must close.
async fn write_or_close(wire: &mut Wire, reply: &str) -> bool {
    wire.write_all(reply.as_bytes()).await.is_err()
}

/// Splits `VERB rest...` into the upper-cased verb and the rest.
fn split_command(line: &str) -> (String, &str) {
    match line.split_once(' ') {
        Some((verb, rest)) => (verb.to_ascii_uppercase(), rest),
        None => (line.to_ascii_uppercase(), ""),
    }
}

/// Redacts credential material from AUTH command lines before they are recorded.
fn redact_auth(line: &str) -> String {
    let mut words = line.splitn(2, ' ');
    let verb = words.next().unwrap_or_default();
    let rest = words.next().unwrap_or_default();
    if verb.eq_ignore_ascii_case("AUTH") {
        let mechanism = rest.split_whitespace().next().unwrap_or_default();
        format!("AUTH {mechanism} [redacted]")
    } else {
        line.to_owned()
    }
}

/// The address between angle brackets in a MAIL/RCPT argument.
fn angle_arg(args: &str) -> Option<String> {
    let open = args.find('<')?;
    let rest = &args[open + 1..];
    let close = rest.find('>')?;
    Some(rest[..close].to_owned())
}

/// Reverses SMTP dot-stuffing on one raw line, terminator included.
fn unescape_dots(line: &str) -> String {
    match line.strip_prefix("..") {
        Some(rest) => format!(".{rest}"),
        None => line.to_owned(),
    }
}
