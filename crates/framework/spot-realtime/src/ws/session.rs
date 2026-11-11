//! One connection: the auth-first handshake, the room commands over the bus, and the
//! liveness rules — exactly `contracts/native-api/realtime.md`.
//!
//! The connection runs as one task owning both the socket and the bus
//! [`Subscription`](crate::Subscription); its select loop only ever awaits one share of the
//! work per branch (inbound frames, the bus queue, the heartbeat, the idle deadline,
//! shutdown, session expiry), and no lock is held across an await. Outbound writes carry a
//! stall window: a client that
//! stops *reading* gets the treatment a slow consumer gets from the bus — close 4008 —
//! rather than pinning the task to a dead socket, because ADR-25 says disconnect, never wait.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket};
use spot_core::SiteId;

use crate::bus::RealtimeBus;
use crate::error::{Closed, JoinError};
use crate::event::Principal;
use crate::room::Room;

use super::protocol::{
    ClientMessage, JoinOutcomeReport, MAX_FRAME_BYTES, MAX_ROOMS, ServerMessage, WireError,
    event_frame, wire_room,
};
use super::verifier::TokenVerifier;

/// Why one connection task stopped: the close frame went out (or the peer is already gone).
enum Stop {
    Closed,
}

/// After how long without any inbound frame — pong replies included — the server closes
/// the connection: contract's 2 × `heartbeat_seconds`. A pong frame is part of "sending":
/// a quiet browser's automatic pong reply is the only signal it gives, so post-auth it
/// counts as liveness (lead review on WP-PLT-08.T2).
fn idle_window(heartbeat_seconds: u64) -> Duration {
    Duration::from_secs(2 * heartbeat_seconds.max(1))
}

/// How long one outbound write (including a close frame) may take before the client is
/// treated as not reading fast enough. Same window as the idle rule.
fn send_window(heartbeat_seconds: u64) -> Duration {
    idle_window(heartbeat_seconds)
}

/// The close codes of the contract's table, plus the plain 1000 for an idle close, which the
/// table does not single out (an idle close is a clean close: the client reconnects and
/// resumes as after any drop).
pub(crate) mod close {
    pub(super) const NORMAL: u16 = 1000;
    pub(super) const SHUTTING_DOWN: u16 = 1001;
    pub(super) const BINARY_FRAME: u16 = 1003;
    pub(super) const PROTOCOL: u16 = 1008;
    pub(super) const TOO_BIG: u16 = 1009;
    pub(super) const AUTH: u16 = 4001;
    pub(super) const SLOW_CONSUMER: u16 = 4008;
    pub(super) const SESSION_EXPIRED: u16 = 4009;
}

/// How one read of the socket came back.
enum Inbound {
    /// A text frame — the only frame the protocol speaks (besides pings for liveness).
    Text(String),
    /// A client-sent WebSocket ping: counts as liveness, is not a protocol message.
    Liveness,
    /// A WebSocket pong, a reply to our heartbeat ping. Post-auth it counts as liveness
    /// (a sending frame counts — a quiet browser only ever answers our pings); pre-auth it
    /// does not extend the `auth` window.
    Pong,
    /// The client closed its side (or the transport is gone); axum answers the close
    /// handshake for us.
    Ended,
    /// A frame the protocol forbids: close the socket with this code and reason.
    BadFrame(u16, &'static str),
}

/// Reads one frame, mapping what tungstenite reports onto protocol close codes. Frame sizes
/// over the contract's 64 KiB never get buffered in the first place — the upgrade pins the
/// library's own `max_frame_size`/`max_message_size` to [`MAX_FRAME_BYTES`] — and a frame
/// that slips past still trips the size check here. A capacity error (the library refusing
/// a frame, or the frame arriving whole but over the limit) is the contract's 1009;
/// anything else broken (invalid utf-8, a failed handshake state, ...) is a protocol
/// violation (1008). A binary frame is 1003 by the contract.
async fn read_inbound(socket: &mut WebSocket) -> Inbound {
    let Some(raw) = socket.recv().await else {
        return Inbound::Ended;
    };
    match raw {
        Ok(message) => classify(message),
        Err(error) => {
            if is_capacity_reject(&error) {
                // The transport refused the frame on size before anything else could
                // happen; the contract's table gives this exactly one close code.
                Inbound::BadFrame(close::TOO_BIG, "a frame over 64 KiB")
            } else {
                Inbound::BadFrame(close::PROTOCOL, "a frame the protocol cannot accept")
            }
        }
    }
}

/// Sorts one decodable frame into the inbound vocabulary.
fn classify(message: Message) -> Inbound {
    match message {
        Message::Text(text) if text.len() > MAX_FRAME_BYTES => {
            // The frame is buffered but unusable under the contract: drop it and tell why.
            let _ = text;
            Inbound::BadFrame(close::TOO_BIG, "a frame over 64 KiB")
        }
        Message::Text(text) => Inbound::Text(text.as_str().to_owned()),
        Message::Ping(_) => Inbound::Liveness,
        Message::Pong(_) => Inbound::Pong,
        Message::Binary(_) => {
            Inbound::BadFrame(close::BINARY_FRAME, "binary frames are not spoken")
        }
        Message::Close(_) => Inbound::Ended,
    }
}

/// Whether the socket error is tungstenite's capacity rejection — a frame (or message)
/// larger than the configured limit. The axum error is an opaque box, so the test is the
/// error's message; a `1009` close is the contract's answer exactly for this case.
fn is_capacity_reject(error: &axum::Error) -> bool {
    error.to_string().starts_with("Space limit exceeded")
}

/// How long the close lingers before the socket drops, so the peer finds the close frame
/// behind the heartbeat pings it never read (a reset there would swallow the close code).
const CLOSE_LINGER: Duration = Duration::from_secs(1);

/// Sends a close frame. Best effort: a peer that already vanished (or stopped reading) makes
/// the send fail or stall; the stall window bounds the wait, and the socket is dropped
/// either way — that drop is what actually disconnects. Before the drop the close lingers
/// ([`CLOSE_LINGER`]): the peer — very possibly a client that has stopped reading its
/// heartbeat pings, all of them still queued ahead of the close — then finds the close
/// frame behind them instead of a connection reset, so the contract's close code is the one
/// it sees. The linger ends early when the peer answers the close or hangs up (it has then
/// read the frame), so a well-behaved peer does not hold the socket for the whole linger; a
/// peer that never answers still gets all of it.
async fn close_socket(socket: &mut WebSocket, code: u16, reason: &'static str, window: Duration) {
    let frame = Message::Close(Some(CloseFrame {
        code,
        reason: Utf8Bytes::from_static(reason),
    }));
    if tokio::time::timeout(window, socket.send(frame))
        .await
        .is_err()
    {
        tracing::debug!(
            code,
            "close frame did not go out in time; dropping the socket"
        );
    }
    let _ = tokio::time::timeout(CLOSE_LINGER, wait_for_peer_close(socket)).await;
}

/// Reads and discards frames until the peer's close reply arrives or the transport ends.
async fn wait_for_peer_close(socket: &mut WebSocket) {
    loop {
        match socket.recv().await {
            None | Some(Err(_) | Ok(Message::Close(_))) => return,
            Some(Ok(_)) => {}
        }
    }
}

/// Sends one JSON message with the stall window applied.
async fn send_text(socket: &mut WebSocket, text: String, window: Duration) -> Result<(), Stop> {
    let frame = Message::text(text);
    match tokio::time::timeout(window, socket.send(frame)).await {
        Err(_) => {
            // The client stopped reading TCP: not reading fast enough — the same client
            // behaviour contract 4008 names. Tell it why; the socket closes either way.
            tracing::debug!("outbound write stalled; closing as a slow consumer");
            close_socket(socket, close::SLOW_CONSUMER, "slow consumer", window).await;
            Err(Stop::Closed)
        }
        Ok(Err(_error)) => Err(Stop::Closed),
        Ok(Ok(())) => Ok(()),
    }
}

/// Serializes and sends one [`ServerMessage`]; an encoding failure (impossible in practice
/// for these types) drops the frame rather than the connection.
async fn send_message(
    socket: &mut WebSocket,
    message: &ServerMessage,
    window: Duration,
) -> Result<(), Stop> {
    let text = match serde_json::to_string(message) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(%error, "dropping server frame: encoding failed");
            return Ok(());
        }
    };
    send_text(socket, text, window).await
}

/// What a completed handshake hands the session.
struct Handshake {
    /// The authenticated principal: `ready.user`, the auto-joined room, and the bus's
    /// authorisation identity.
    principal: Principal,
    /// The connection's bus subscription (freshly subscribed; in no rooms yet).
    subscription: crate::Subscription,
    /// The auto-joined `user:<id>` room; `None` when the principal id is not a wire-valid
    /// user id (a verifier contract breach, logged) — at worst the user room's notifications
    /// are missed, which ADR-25 permits.
    user_room: Option<Room>,
    /// The rooms the client is in; seeded by the auto-join. Counts against the contract's
    /// 200-room limit.
    rooms: HashSet<String>,
    /// The expiry signal line: the watcher flips it to `true` once a watched session is
    /// confirmed gone; the sending side stays alive for the connection, so the receiving
    /// side of `expiry_rx` never errs — including for a connection whose verifier provides
    /// no watch at all.
    expiry_tx: tokio::sync::watch::Sender<bool>,
    expiry_rx: tokio::sync::watch::Receiver<bool>,
    /// Aborts the expiry-watcher task when the connection ends, on every exit path.
    watcher: WatcherGuard,
}

/// Aborts the expiry-watcher task when the connection's state dies.
struct WatcherGuard(Option<tokio::task::JoinHandle<()>>);

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// A shared per-router state the site-route handler fans out to connections.
pub(crate) struct Shared {
    /// The bus events flow through.
    pub(crate) bus: RealtimeBus,
    /// The `auth` token checker.
    pub(crate) verifier: Arc<dyn TokenVerifier>,
    /// Heartbeat cadence in seconds (contract default 25).
    pub(crate) heartbeat_seconds: u64,
    /// Cancelled when the server is shutting down: connections close with 1001.
    pub(crate) shutdown: tokio_util::sync::CancellationToken,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("heartbeat_seconds", &self.heartbeat_seconds)
            .finish()
    }
}

/// The auth-first handshake: exactly one `auth` within 10 s (fixed by the contract),
/// verified by the [`TokenVerifier`]. Every failure path sends the contract's close code and
/// ends the connection here, so the session below only ever starts authenticated; likewise
/// the expiry watcher is started (or not) exactly once, here.
async fn authenticate(
    shared: &Shared,
    site: &SiteId,
    socket: &mut WebSocket,
    window: Duration,
) -> Option<Handshake> {
    // The 10 s window for the first `auth`, straight from the upgrade. WebSocket pings and
    // pong frames from the client do not extend it (a pong is "sending" only after the
    // auth); only an `auth` can stop it.
    let deadline = tokio::time::sleep(super::AUTH_TIMEOUT);
    tokio::pin!(deadline);
    let (session_tx, expiry_rx) = tokio::sync::watch::channel(false);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                tracing::debug!("realtime connection closed: no auth arrived in time");
                close_socket(socket, close::AUTH, "auth timed out", window).await;
                return None;
            }
            _ = shared.shutdown.cancelled() => {
                close_socket(socket, close::SHUTTING_DOWN, "server shutting down", window).await;
                return None;
            }
            first = read_inbound(socket) => {
                let authenticate_now = match first {
                    Inbound::Ended => None,
                    // A pong or a ping instead of auth: not an `auth`; the deadline keeps
                    // running (the loop re-polls it). Pre-auth no frame stops the auth
                    // clock — not even a pong: liveness there is the contract's `auth`
                    // itself, due within its 10 s.
                    Inbound::Pong | Inbound::Liveness => Some(()),
                    Inbound::BadFrame(code, reason) => {
                        close_socket(socket, code, reason, window).await;
                        return None;
                    }
                    Inbound::Text(frame_text) => match serde_json::from_str::<ClientMessage>(&frame_text) {
                        // The one message the handshake speaks.
                        Ok(ClientMessage::Auth { token }) => {
                        // The verifier may await a session store: it runs inside the
                        // same auth window (and the shutdown signal), so a slow one
                        // closes exactly like a missing `auth`.
                        let verdict = tokio::select! {
                            _ = &mut deadline => {
                                tracing::debug!("realtime connection closed: verification did not finish in time");
                                close_socket(socket, close::AUTH, "auth timed out", window).await;
                                return None;
                            }
                            _ = shared.shutdown.cancelled() => {
                                close_socket(socket, close::SHUTTING_DOWN, "server shutting down", window).await;
                                return None;
                            }
                            verdict = shared.verifier.verify(site, &token) => verdict,
                        };
                        match verdict {
                            Ok(principal) => {
                                return Some(build_handshake(shared, site, principal, token, session_tx, expiry_rx));
                            }
                            Err(rejected) => {
                                tracing::debug!(reason = rejected.reason(), "realtime auth rejected");
                                close_socket(socket, close::AUTH, "authentication failed", window).await;
                                return None;
                            }
                        }
                        }
                        // A valid protocol message before `auth` is the contract's "a
                        // message before auth" violation; a frame that does not parse at
                        // all is invalid JSON or an unknown `type`. Both are 1008.
                        Ok(_) | Err(_) => {
                            close_socket(socket, close::PROTOCOL, "the first message must be auth", window).await;
                            return None;
                        }
                    },
                };
                authenticate_now?;
            }
        }
    }
}

/// Wraps a successful verify: subscribes to the bus and starts the session-expiry watcher
/// when the verifier can watch the session. The user room is only auto-joined when the
/// principal id is a wire-valid user id (the native API contract's verifiers return
/// `RecordId`s); anything else logs and continues without it.
fn build_handshake(
    shared: &Shared,
    site: &SiteId,
    principal: Principal,
    token: String,
    session_tx: tokio::sync::watch::Sender<bool>,
    expiry_rx: tokio::sync::watch::Receiver<bool>,
) -> Handshake {
    let watcher = shared.verifier.expiry_watch(&token);
    let handle = watcher.map(|watch| {
        let verifier = shared.verifier.clone();
        let site = site.clone();
        let session_tx = session_tx.clone();
        tokio::spawn(async move {
            loop {
                // Wait for the implementation's signal, then ask it again: resolving the
                // watch is a hint — the session is only gone when `verify` now rejects (the
                // watch may resolve for a permission change that leaves the session valid).
                watch.wait().await;
                if let Err(rejected) = verifier.verify(&site, &token).await {
                    tracing::debug!(
                        reason = rejected.reason(),
                        "realtime session expired or revoked"
                    );
                    let _ = session_tx.send(true);
                    return;
                }
            }
        })
    });
    let user_room_name = format!("user:{}", principal.id());
    let user_room = wire_room(&user_room_name);
    if user_room.is_none() {
        tracing::warn!(
            user = principal.id(),
            "principal id is not a wire room id; not auto-joining a user room"
        );
    }
    Handshake {
        subscription: shared.bus.subscribe(site.clone(), principal.clone()),
        principal,
        user_room,
        rooms: HashSet::new(),
        expiry_rx,
        expiry_tx: session_tx,
        watcher: WatcherGuard(handle),
    }
}

/// Runs one realtime WebSocket connection to its end: auth-first handshake (the 4001 paths),
/// the `ready` reply, the auto-joined `user:<id>` room, the room commands, event delivery,
/// heartbeats, idle and slow-consumer closes, session-expiry watching, and server shutdown.
/// The pieces (socket, subscription, room set, expiry line) stay plain locals of this task —
/// the select loop borrows each one separately, which is what keeps the borrows disjoint.
pub(crate) async fn run(
    shared: Arc<Shared>,
    site: SiteId,
    mut socket: WebSocket,
    lease: Option<spot_http::server::ConnectionLease>,
) {
    // Held until the session ends on every exit path: the server counts this connection
    // (`ServerHandle::open_connections`) for as long as the lease lives.
    let _lease = lease;
    let Some(handshake) = authenticate(
        &shared,
        &site,
        &mut socket,
        send_window(shared.heartbeat_seconds),
    )
    .await
    else {
        return; // the handshake closed the socket, or the server is shutting down
    };
    let Handshake {
        principal,
        subscription,
        user_room,
        rooms,
        expiry_tx,
        expiry_rx,
        watcher,
    } = handshake;
    // These two are held for their side effects: the expiry line must outlive the main loop
    // (so its receiver never errs), and the watcher task aborts when this frame dies. Both
    // drop at the end of `run` on every exit path.
    let _expiry_tx = expiry_tx;
    let _watcher = watcher;
    // `ready` carries the heartbeat cadence the client may rely on.
    let ready = send_message(
        &mut socket,
        &ServerMessage::Ready {
            user: principal.id().to_owned(),
            heartbeat_seconds: shared.heartbeat_seconds,
        },
        send_window(shared.heartbeat_seconds),
    )
    .await;
    if ready.is_err() {
        return;
    }
    // One period after the upgrade, not at interval-creation time (interval's first tick
    // otherwise completes immediately).
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(shared.heartbeat_seconds.max(1)),
        Duration::from_secs(shared.heartbeat_seconds.max(1)),
    );
    let mut subscription = subscription;
    let mut rooms = rooms;
    let mut last_activity = tokio::time::Instant::now();
    let mut expiry_rx = expiry_rx;
    join_auto_user_room(&mut subscription, &mut rooms, user_room).await;
    loop_until_closed(
        &shared,
        &mut socket,
        &mut subscription,
        &mut rooms,
        &mut last_activity,
        &mut heartbeat,
        &mut expiry_rx,
    )
    .await;
}

/// Auto-joins `user:<id>` right after `ready`. The bus authorizes it like any join; a denial
/// (or an unusable principal id, see [`Handshake::user_room`]) is logged and the connection
/// continues — ADR-25: no correctness may depend on a notification arriving.
async fn join_auto_user_room(
    subscription: &mut crate::Subscription,
    rooms: &mut HashSet<String>,
    user_room: Option<Room>,
) {
    let Some(room) = user_room else {
        return;
    };
    match subscription.join(&room, None).await {
        Ok(_) => {
            rooms.insert(room.as_str().to_owned());
        }
        Err(why) => {
            tracing::warn!(room = %room, %why, "user room join failed; continuing without it");
        }
    }
}

/// The connection's main loop: inbound protocol messages, delivered events, the heartbeat,
/// idle expiry, session expiry, and server shutdown — the first cause closes and ends. Every
/// exit either sent the contract's close for its cause, or the peer was already gone.
///
/// Idleness is not polled off the heartbeat tick: [`idle_window`] is the exact time an
/// otherwise-quiet connection may last, so the loop watches it as its own
/// `sleep_until(last_activity + window)` arm, and a pong — a quiet browser's only answer to
/// our heartbeat pings — pushes that deadline like any other inbound frame. The close then
/// lands within a tick of 2 × `heartbeat_seconds` after the last frame, not up to a whole
/// period later.
///
/// The loop borrows each piece separately (`socket`, `subscription`, `rooms`, …); the select
/// polls them concurrently and the handlers use them one at a time, so no two borrows of a
/// piece ever overlap.
async fn loop_until_closed(
    shared: &Shared,
    socket: &mut WebSocket,
    subscription: &mut crate::Subscription,
    rooms: &mut HashSet<String>,
    last_activity: &mut tokio::time::Instant,
    heartbeat: &mut tokio::time::Interval,
    expiry_rx: &mut tokio::sync::watch::Receiver<bool>,
) {
    loop {
        let window = send_window(shared.heartbeat_seconds);
        let idle = tokio::time::sleep_until(*last_activity + window);
        tokio::pin!(idle);
        tokio::select! {
            // The heartbeat: ping the client once a period. (Idleness has its own arm, so a
            // tick does not need to re-check any elapsed time.)
            _ = heartbeat.tick() => {
                let window = send_window(shared.heartbeat_seconds);
                let ping = tokio::time::timeout(
                    window,
                    socket.send(Message::Ping(axum::body::Bytes::new())),
                )
                .await;
                match ping {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return, // the transport is gone; nothing to tell
                    Err(_) => {
                        // The client cannot take a ping: it stopped reading.
                        tracing::debug!("heartbeat ping stalled; closing as a slow consumer");
                        stop(socket, close::SLOW_CONSUMER, "slow consumer", window).await;
                        return;
                    }
                }
            }
            // Two heartbeats of silence — no frame answered our last pings, pongs included
            // (they count as sending): close, and let the client reconnect and resume.
            _ = &mut idle => {
                tracing::debug!("realtime connection closed: idle past the heartbeat window");
                stop(socket, close::NORMAL, "idle", window).await;
                return;
            }
            // Events from the bus: the stream of every joined room, `seq` per room.
            delivered = subscription.recv() => {
                let window = send_window(shared.heartbeat_seconds);
                match delivered {
                    Ok(event) => {
                        subscription.sync_rooms(rooms);
                        let Some(message) = event_frame(event) else {
                            continue; // not emittable under the contract; logged in event_frame
                        };
                        if send_message(socket, &message, window).await.is_err() {
                            return;
                        }
                    }
                    Err(Closed::SlowConsumer) => {
                        // The bus disconnects a client whose queue filled: contract 4008.
                        stop(socket, close::SLOW_CONSUMER, "slow consumer", window).await;
                        return;
                    }
                    Err(Closed::ChannelClosed) => {
                        // The bus itself went away: report it the way a shutdown reads.
                        stop(socket, close::SHUTTING_DOWN, "server shutting down", window).await;
                        return;
                    }
                }
            }
            // Inbound protocol messages and liveness. Every decodable frame counts as
            // activity — pongs included: a browser tab that only answers our heartbeat
            // pings stays connected.
            inbound = read_inbound(socket) => match inbound {
                Inbound::Ended => return,
                Inbound::Pong => {
                    // A send-frame counts as sending (lead review on WP-PLT-08.T2). The
                    // next loop turn rebuilds the idle deadline from here.
                    *last_activity = tokio::time::Instant::now();
                }
                Inbound::BadFrame(code, reason) => {
                    let window = send_window(shared.heartbeat_seconds);
                    stop(socket, code, reason, window).await;
                    return;
                }
                Inbound::Liveness => {
                    *last_activity = tokio::time::Instant::now();
                }
                Inbound::Text(frame_text) => {
                    *last_activity = tokio::time::Instant::now();
                    match serde_json::from_str::<ClientMessage>(&frame_text) {
                        Err(_) => {
                            // Invalid JSON or an unknown `type`: 1008.
                            let window = send_window(shared.heartbeat_seconds);
                            stop(socket, close::PROTOCOL, "unknown or malformed message", window).await;
                            return;
                        }
                        Ok(message) => {
                            if dispatch(shared, socket, subscription, rooms, message)
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            },
            // The watcher confirmed the session expired or was revoked: 4009.
            _ = expiry_rx.changed() => {
                if *expiry_rx.borrow() {
                    let window = send_window(shared.heartbeat_seconds);
                    stop(socket, close::SESSION_EXPIRED, "session expired or revoked", window).await;
                    return;
                }
            }
            // Server shutdown: 1001, then the client reconnects with resume.
            _ = shared.shutdown.cancelled() => {
                let window = send_window(shared.heartbeat_seconds);
                stop(socket, close::SHUTTING_DOWN, "server shutting down", window).await;
                return;
            }
        }
    }
}

/// The last thing a connection does on any exit: send the close frame (best effort).
async fn stop(socket: &mut WebSocket, code: u16, reason: &'static str, window: Duration) {
    close_socket(socket, code, reason, window).await;
}

/// An authenticated, parsed protocol message. `auth` again is a protocol violation
/// (first-message-only); the rest is room bookkeeping or liveness.
async fn dispatch(
    _shared: &Shared,
    socket: &mut WebSocket,
    subscription: &mut crate::Subscription,
    rooms: &mut HashSet<String>,
    message: ClientMessage,
) -> Result<(), Stop> {
    let window = send_window(_shared.heartbeat_seconds);
    // The host may have evicted this connection from rooms since the last command: make the
    // mirrored set true before it is consulted (repeat-join check, `MAX_ROOMS`).
    subscription.sync_rooms(rooms);
    match message {
        ClientMessage::Auth { .. } => {
            stop(
                socket,
                close::PROTOCOL,
                "auth is only ever the first message",
                window,
            )
            .await;
            Err(Stop::Closed)
        }
        ClientMessage::Ping => send_message(socket, &ServerMessage::Pong, window).await,
        ClientMessage::Leave { room } => {
            leave(subscription, rooms, &room);
            Ok(())
        }
        ClientMessage::Join {
            client_ref,
            room,
            resume_after,
        } => {
            join(
                socket,
                subscription,
                rooms,
                client_ref,
                room,
                resume_after,
                window,
            )
            .await
        }
    }
}

/// Runs one `join` command: repeats are `joined` again with no side effects (no
/// re-authorisation, no replay); the contract's 200-room cap fails before anything else;
/// then the room is wire-validated and [`Subscription::join`](crate::Subscription) — whose
/// authorizer runs on every join — maps onto `joined` / `replayed` / `resync_required` /
/// `join_error`.
async fn join(
    socket: &mut WebSocket,
    subscription: &mut crate::Subscription,
    rooms: &mut HashSet<String>,
    client_ref: String,
    room: String,
    resume_after: Option<u64>,
    window: Duration,
) -> Result<(), Stop> {
    if rooms.contains(&room) {
        // The contract's repeat-join rule: reply `joined` again, no side effects — no fresh
        // authorisation, no replay of anything.
        return send_message(
            socket,
            &ServerMessage::Joined {
                client_ref,
                room,
                outcome: JoinOutcomeReport::Joined,
                replayed: None,
            },
            window,
        )
        .await;
    }
    if rooms.len() >= MAX_ROOMS {
        return send_message(
            socket,
            &ServerMessage::JoinError {
                client_ref,
                room,
                error: WireError {
                    code: "realtime.too_many_rooms",
                    message: format!("the connection is already in its {MAX_ROOMS} rooms."),
                },
            },
            window,
        )
        .await;
    }
    let parsed = match wire_room(&room) {
        Some(room) => room,
        None => {
            return send_message(
                socket,
                &ServerMessage::JoinError {
                    client_ref,
                    room,
                    error: WireError {
                        code: "realtime.unknown_room",
                        message: "no such room: the name is not one of the contract's rooms."
                            .to_owned(),
                    },
                },
                window,
            )
            .await;
        }
    };
    // The authorizer runs inside the bus's join (outside any of the bus's locks).
    match subscription.join(&parsed, resume_after).await {
        Ok(outcome) => {
            // The replayed events are already flowing into the subscription's queue — they
            // follow this reply in room-`seq` order (the contract promises no ordering
            // across rooms).
            rooms.insert(room.clone());
            let (outcome, replay) = match outcome {
                crate::JoinOutcome::Joined => (JoinOutcomeReport::Joined, None),
                crate::JoinOutcome::Replayed { count } => {
                    (JoinOutcomeReport::Replayed, Some(count as u64))
                }
                crate::JoinOutcome::ResyncRequired => (JoinOutcomeReport::ResyncRequired, None),
            };
            send_message(
                socket,
                &ServerMessage::Joined {
                    client_ref,
                    room,
                    outcome,
                    replayed: replay,
                },
                window,
            )
            .await
        }
        Err(JoinError::Denied(denied)) => {
            send_message(
                socket,
                &ServerMessage::JoinError {
                    client_ref,
                    room,
                    error: WireError {
                        code: "permission.denied",
                        message: denied.reason().to_owned(),
                    },
                },
                window,
            )
            .await
        }
        Err(JoinError::Closed) => {
            // The subscription died while joining (the replay did not fit the queue, or the
            // bus closed it under us): nothing else works on this connection.
            tracing::debug!(room = %parsed, "subscription closed during join");
            stop(socket, close::SLOW_CONSUMER, "slow consumer", window).await;
            Err(Stop::Closed)
        }
    }
}

/// Runs one `leave` command: always silent — even for a room name that is not a room.
fn leave(subscription: &mut crate::Subscription, rooms: &mut HashSet<String>, room: &str) {
    if let Some(parsed) = wire_room(room) {
        subscription.leave(&parsed);
        rooms.remove(room);
    }
}
