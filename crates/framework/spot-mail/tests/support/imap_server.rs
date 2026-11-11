//! The fake IMAP4rev1 server the spot-mail ingestion tests run against: holds a scripted mailbox
//! of fixture messages, records every session as seen on the wire (LOGIN secrets redacted), and
//! serves TLS (implicit or via STARTTLS) with a self-signed certificate generated at startup. It
//! speaks just enough of the protocol for async-imap's client: greeting, CAPABILITY, STARTTLS,
//! LOGIN, SELECT with UIDVALIDITY/UIDNEXT, UID SEARCH, UID FETCH of RFC822.SIZE and BODY.PEEK
//! literals, and LOGOUT. The server answers no state-changing command; every client request is
//! recorded verbatim (tags stripped) so tests can assert that bodies were peeked and that the
//! mailbox was never touched.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;

use super::{Must, NO_REPLY_PARK, Wire, acceptor, read_command, write_or_close};

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
    /// Refuse SELECT with a tagged `NO` ("no such mailbox").
    pub reject_select: bool,
    /// Refuse UID FETCH with a tagged `NO`.
    pub reject_fetch: bool,
    /// Accept the connection but never answer anything; the client times out.
    pub no_reply: bool,
    /// Terminate the connection partway through a UID FETCH — after this many message responses,
    /// before the tagged completion. `0` fetches normally.
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

/// One TLS-or-STARTTLS session as seen on the wire, taken as a snapshot when asserted on. The
/// LOGIN secret never appears here: the user name is recorded, the argument list is not.
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

/// A running fake IMAP server; tests drive the ingestion transport against it.
pub struct ImapFakeServer {
    addr: SocketAddr,
    host_pem: Option<String>,
    script: Arc<Mutex<ImapScript>>,
    records: Arc<Mutex<Vec<Arc<Mutex<ImapRecording>>>>>,
    connections: Arc<AtomicUsize>,
}

impl ImapFakeServer {
    /// The port the transport must point at.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The self-signed certificate PEM, when this server speaks TLS.
    pub fn host_certificate_pem(&self) -> Option<&str> {
        self.host_pem.as_deref()
    }

    /// The number of TCP connections accepted so far.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }

    /// Replaces the script; sessions re-read it at every command boundary, so a change made
    /// between two polls reaches the next poll of a test that keeps the server running.
    pub async fn set_script(&self, script: ImapScript) {
        *self.script.lock().await = script;
    }

    /// Every session seen so far, in connection order, as snapshots.
    pub async fn recordings(&self) -> Vec<ImapRecording> {
        let mut snapshots = Vec::new();
        for recording in self.records.lock().await.iter() {
            snapshots.push(recording.lock().await.clone());
        }
        snapshots
    }

    /// The last session seen, as a snapshot.
    pub async fn last_recording(&self) -> ImapRecording {
        let recording = self.records.lock().await.last().cloned().must();
        recording.lock().await.clone()
    }
}

/// Spawns a fake server that negotiates TLS from the first byte (implicit TLS on the port).
pub async fn spawn_tls() -> ImapFakeServer {
    spawn(true).await
}

/// Spawns a fake server whose greeting and protocol start plain and upgrade via STARTTLS.
pub async fn spawn_starttls() -> ImapFakeServer {
    spawn(false).await
}

async fn spawn(implicit_tls: bool) -> ImapFakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.must();
    let addr = listener.local_addr().must();
    let (acceptor, host_pem) = acceptor();
    let state = ImapState {
        script: Arc::new(Mutex::new(ImapScript::default())),
        records: Arc::new(Mutex::new(Vec::new())),
        connections: Arc::new(AtomicUsize::new(0)),
    };
    tokio::spawn(serve(listener, acceptor, !implicit_tls, state.clone()));
    ImapFakeServer {
        addr,
        host_pem: Some(host_pem),
        script: state.script,
        records: state.records,
        connections: state.connections,
    }
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
        let acceptor = acceptor.clone();
        let state = state.clone();
        tokio::spawn(imap_session(stream, acceptor, starttls, state));
    }
}

/// One protocol phase — from the greeting (and TLS upgrade) to the hangup. Every phase
/// processes commands until the connection breaks, a scripted disconnect, or LOGOUT.
enum ImapPhase {
    Closed,
    Upgraded(Wire),
}

async fn imap_session(stream: TcpStream, acceptor: TlsAcceptor, starttls: bool, state: ImapState) {
    let wire = if starttls {
        Wire::Plain(stream)
    } else {
        match acceptor.accept(stream).await {
            Ok(tls) => Wire::Tls(Box::new(tls)),
            Err(_) => return,
        }
    };
    state
        .records
        .lock()
        .await
        .push(Arc::new(Mutex::new(ImapRecording::default())));
    let recording = state.records.lock().await.last().cloned().must();
    let mut greeted = false;
    let mut wire = wire;
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
            // Hold the connection open without answering anything; the client times out.
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
        // The script is re-read at every command boundary so that a `set_script` between two
        // fetches takes effect on the next poll.
        let script = state.script.lock().await.clone();
        let mut recording = recording.lock().await;
        let (verb, arguments) = match command.split_once(' ') {
            Some((verb, rest)) => (verb.to_ascii_uppercase(), trim_arguments(rest)),
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
                let mut reply = String::from("* CAPABILITY IMAP4rev1 STARTTLS\r\n");
                reply.push_str(&format!("{tag} OK CAPABILITY completed\r\n"));
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
                match acceptor.accept(stream).await {
                    Ok(tls) => return ImapPhase::Upgraded(Wire::Tls(Box::new(tls))),
                    Err(_) => return ImapPhase::Closed,
                }
            }
            "SELECT" => {
                recording
                    .select_mailboxes
                    .push(arguments.trim_matches('"').to_owned());
                let reply = if script.reject_select {
                    format!("{tag} NO no such mailbox\r\n")
                } else {
                    let exists = script.messages.len();
                    let uid_next = script
                        .messages
                        .iter()
                        .map(|message| message.uid)
                        .max()
                        .unwrap_or(0)
                        + 1;
                    let mut reply = format!("* {exists} EXISTS\r\n");
                    reply.push_str(&format!(
                        "* OK [UIDVALIDITY {}] UIDs valid\r\n",
                        script.uid_validity
                    ));
                    reply.push_str(&format!("* OK [UIDNEXT {uid_next}] Predicted next UID\r\n"));
                    reply.push_str("* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n");
                    reply.push_str(&format!("{tag} OK [READ-WRITE] SELECT completed\r\n"));
                    reply
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
                let Some((subcommand, arguments)) = arguments.split_once(' ') else {
                    let reply = format!("{tag} BAD UID needs a subcommand\r\n");
                    if write_or_close(&mut wire, &reply).await {
                        return ImapPhase::Closed;
                    }
                    continue;
                };
                match subcommand.to_ascii_uppercase().as_str() {
                    "SEARCH" => {
                        recording.uid_searches.push(arguments.to_owned());
                        let reply = search_reply(&script, tag, arguments);
                        if write_or_close(&mut wire, &reply).await {
                            return ImapPhase::Closed;
                        }
                    }
                    "FETCH" => {
                        let Some((uid_set, items)) = arguments.split_once(' ') else {
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
                        let wanted = uid_set
                            .split(',')
                            .filter_map(|uid| uid.parse::<u32>().ok())
                            .collect::<std::collections::HashSet<_>>();
                        let mut sorted = script.messages.clone();
                        sorted.sort_by_key(|message| message.uid);
                        let mut answered = 0usize;
                        let mut dropped = false;
                        for message in sorted
                            .iter()
                            .filter(|message| wanted.contains(&message.uid))
                        {
                            answered += 1;
                            let mut part = format!("* {answered} FETCH (UID {}", message.uid);
                            if items.contains("RFC822.SIZE") {
                                part.push_str(&format!(" RFC822.SIZE {}", message.raw.len()));
                            }
                            // The server answers BODY[…] even when the client asked to peek;
                            // which section the client requested is what the tests assert.
                            let literal: &[u8];
                            if items.contains("BODY.PEEK[HEADER]") {
                                literal = header_block(&message.raw);
                                part.push_str(&format!(" BODY[HEADER] {{{}}}", literal.len()));
                            } else if items.contains("BODY.PEEK[]") {
                                literal = &message.raw;
                                part.push_str(&format!(" BODY[] {{{}}}", literal.len()));
                            } else {
                                literal = &[];
                            }
                            let mut bytes = part.into_bytes();
                            if literal.is_empty() {
                                bytes.extend_from_slice(b")\r\n");
                            } else {
                                // A literal continues the line inline: marker, CRLF, data, `)`.
                                bytes.extend_from_slice(b"\r\n");
                                bytes.extend_from_slice(literal);
                                bytes.extend_from_slice(b")\r\n");
                            }
                            if write_bytes(&mut wire, &bytes).await {
                                return ImapPhase::Closed;
                            }
                            if script.drop_after_fetch_responses > 0
                                && answered >= script.drop_after_fetch_responses
                            {
                                // Scripted destructive disconnect: no tagged completion, just EOF.
                                dropped = true;
                                break;
                            }
                        }
                        if dropped {
                            recording.dropped_mid_fetch = true;
                            return ImapPhase::Closed;
                        }
                        let reply = format!("{tag} OK FETCH completed\r\n");
                        if write_or_close(&mut wire, &reply).await {
                            return ImapPhase::Closed;
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

/// Writes literal bytes (outside the line-oriented `write_or_close` helpers) onto the wire.
async fn write_bytes(wire: &mut Wire, bytes: &[u8]) -> bool {
    wire.write_all(bytes).await.is_err()
}

/// The untagged SEARCH answer plus the tagged completion for the criteria the client sent.
fn search_reply(script: &ImapScript, tag: &str, criteria: &str) -> String {
    // `UID {n}:1:*` is the only criteria this client issues; the server answers every UID
    // serving it directly.
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

/// Trims whitespace the client's command spelling may carry around the arguments.
fn trim_arguments(arguments: &str) -> String {
    arguments.trim().to_owned()
}

/// Everything before the blank line, CRLFs of individual headers included, blank line excluded.
fn header_block(raw: &[u8]) -> &[u8] {
    let end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or(raw.len());
    &raw[..end]
}
