//! The inbound side: [`ExternalReceiver`] is the handler behind the peer's webhook path.
//!
//! Order of work, and nothing is skipped or reordered:
//!
//! 1. the path must be the peer's;
//! 2. **the body is read under the peer's cap** ([`ExternalReceiver::read`] /
//!    [`InboundRequest::from_stream`]): a declared `Content-Length` over the cap is refused
//!    before one byte is read, a chunked or header-less body is counted as it arrives and the
//!    read stops one byte past the cap, so an unauthenticated sender cannot exhaust memory
//!    before this handler runs. The bytes received are then checked again against the
//!    same cap — never a header — and an oversized body is refused and counted before anything
//!    is hashed or parsed;
//! 3. **the signature is verified** (constant-time, timestamp window) with a secret resolved
//!    from the vault for this call only; a request that fails is counted and answered, and goes
//!    no further, so nothing unsigned is parsed, mapped, stored or counted as business data;
//! 4. the link must be active (revocation stops inbound as it stops outbound);
//! 5. the body is read in the peer's format: a business acknowledgement goes to the outbox, a
//!    message goes to the inbox;
//! 6. a message becomes a platform [`Message`] under a deterministic id (so a replay of the
//!    peer's id is recognised), numbered in arrival order, signed by the bridge identity, and
//!    handed to [`Inbox::receive`], which applies the same `Link::allows`, grant, schema range,
//!    field list, dedupe and audit as for a tenant peer.
//!
//! Refusals are counted per peer per window ([`RefusalCounter`]), so an unauthenticated sender
//! cannot write one audit row per request: one detail row opens a window, one summary row with
//! the refusals after it closes the window at the first request afterwards.

use spot_audit_chain::PayloadValue;
use spot_connectors::CredentialVault;
use spot_link_exchange::{
    Inbox, InboxStore, Lifetimes, Message, MessageParts, Outbox, OutboxStore, ReceiveStatus,
    RejectCode, TransportParty, canonicalize,
};
use spot_tenant_link::{
    AuditSink, IdentitySigner, Link, LinkEvent, LinkEventKind, LinkState, record_event,
};

use crate::error::{Error, Result};
use crate::peer::ExternalPeer;
use crate::refusals::RefusalCounter;
use crate::sender::apply_ack;
use crate::wire::{Inbound, InboundMessage, decode_inbound, external_message_id};

/// One request to the peer's webhook path, as the HTTP edge hands it over.
///
/// A request exists only through [`InboundRequest::from_stream`] (or, for this peer, the
/// route's [`ExternalReceiver::read`], which passes the peer's own cap): the body is read under
/// a byte cap as it arrives, so an unauthenticated sender cannot make the edge buffer more than
/// the peer accepts before the receiver runs. A constructor over an already-buffered
/// `Vec` exists for tests only.
#[derive(Clone, PartialEq, Eq)]
pub struct InboundRequest {
    /// The request path.
    pub path: String,
    /// The headers (names compared without regard to case).
    pub headers: Vec<(String, String)>,
    /// The raw body, exactly as received: the signature covers these bytes.
    ///
    /// Private on purpose: a `Vec` is already buffered, so a public field would let an edge
    /// fill memory first and check the cap afterwards. Read it through
    /// [`InboundRequest::body`].
    body: Vec<u8>,
}

impl InboundRequest {
    /// Reads one inbound request from the chunks the edge received, under `cap` bytes.
    ///
    /// This is the only way a request comes into being outside tests:
    ///
    /// * a declared `Content-Length` above `cap` — or one that is not a whole number of bytes —
    ///   is refused **before** one chunk is pulled, so a header cannot buy memory either;
    /// * a chunked or header-less body is counted as it arrives: the read stops one byte past
    ///   `cap` and the buffer never grows past `cap`, so the sender's stream decides neither
    ///   the allocation nor how long the read lasts;
    /// * a body of exactly `cap` bytes is accepted, because the receiver refuses only what is
    ///   larger.
    ///
    /// `cap` must be the peer's own [`crate::ExternalPeer::max_body_bytes`] (see
    /// [`ExternalReceiver::read`]); a caller that has no cap has no business reading a body.
    ///
    /// # Errors
    /// [`Error::BodyTooLarge`] when a declared length or the bytes read pass `cap`;
    /// [`Error::InvalidBody`] with a fixed reason when a declared length is not a whole number
    /// of bytes or a chunk cannot be read. Nothing the peer sent is in either error.
    pub fn from_stream<I, C>(
        path: impl Into<String>,
        headers: Vec<(String, String)>,
        body: I,
        cap: usize,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = std::io::Result<C>>,
        C: AsRef<[u8]>,
    {
        let declared = declared_length(&headers)?;
        if declared.is_some_and(|declared| declared > cap) {
            return Err(Error::BodyTooLarge);
        }
        Ok(Self {
            path: path.into(),
            headers,
            body: read_capped(body, cap)?,
        })
    }

    /// A request over an already-buffered body: **tests only**.
    ///
    /// A product build has no such constructor, so no edge can buffer a body and check its size
    /// afterwards: the read itself is bounded ([`InboundRequest::from_stream`]).
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn new(path: impl Into<String>, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            headers,
            body,
        }
    }

    /// The bytes as received: the signature covers exactly these.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// The `Content-Length` the request declares, when it declares a readable one.
///
/// A header that repeats is checked at each occurrence, and an unreadable one is refused
/// rather than ignored: a length the reader cannot trust is not the same as no length.
fn declared_length(headers: &[(String, String)]) -> Result<Option<usize>> {
    let mut declared = None;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let bytes = value
            .trim()
            .parse::<u64>()
            .map_err(|_| Error::InvalidBody {
                reason: "the declared length is not a whole number of bytes",
            })?;
        let bytes = usize::try_from(bytes).map_err(|_| Error::BodyTooLarge)?;
        declared = Some(declared.map_or(bytes, |seen: usize| seen.max(bytes)));
    }
    Ok(declared)
}

/// Reads at most `cap` bytes of `chunks`, refusing the first byte past the cap.
///
/// The buffer is reserved exactly to what the cap allows, never past it, so a body of any size
/// costs at most `cap` bytes of memory here. The stream is left unread at the first chunk that
/// does not fit.
fn read_capped<I, C>(chunks: I, cap: usize) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = std::io::Result<C>>,
    C: AsRef<[u8]>,
{
    let mut body: Vec<u8> = Vec::new();
    for chunk in chunks {
        let chunk = chunk.map_err(|_| Error::InvalidBody {
            reason: "the body could not be read",
        })?;
        let chunk = chunk.as_ref();
        let fits = chunk.len().min(cap.saturating_sub(body.len()));
        let wanted = body.len() + fits;
        if wanted > body.capacity() {
            let target = wanted.saturating_mul(2).min(cap);
            body.reserve_exact(target - body.len());
        }
        body.extend_from_slice(&chunk[..fits]);
        if fits < chunk.len() {
            return Err(Error::BodyTooLarge);
        }
    }
    Ok(body)
}

impl std::fmt::Debug for InboundRequest {
    /// Headers carry the signature and the body is business data: neither is printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundRequest")
            .field("path", &self.path)
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// What the edge answers the peer with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundResponse {
    /// The HTTP status.
    pub status: u16,
    /// The stable code, when the request was refused.
    pub code: Option<&'static str>,
    /// A fixed JSON body; it never echoes anything the peer sent.
    pub body: String,
}

impl InboundResponse {
    fn ok(status: u16, word: &str) -> Self {
        Self {
            status,
            code: None,
            body: format!("{{\"status\":\"{word}\"}}"),
        }
    }

    fn refused(status: u16, code: &'static str) -> Self {
        Self {
            status,
            code: Some(code),
            body: format!("{{\"error\":{{\"code\":\"{code}\"}}}}"),
        }
    }
}

fn micros(now_ms: u64) -> i64 {
    i64::try_from(now_ms)
        .unwrap_or(i64::MAX / 1_000)
        .saturating_mul(1_000)
}

/// The inbound carrier of one external link.
pub struct ExternalReceiver<'a, 'o> {
    peer: &'a ExternalPeer,
    inbox: &'a Inbox<'a>,
    inbox_store: &'a dyn InboxStore,
    outbox: &'a Outbox<'o>,
    outbox_store: &'a dyn OutboxStore,
    bridge: &'a dyn IdentitySigner,
    vault: &'a dyn CredentialVault,
    lifetimes: &'a Lifetimes,
    audit: &'a dyn AuditSink,
    refusals: &'a RefusalCounter,
}

/// The ports an [`ExternalReceiver`] works through.
pub struct ReceiverPorts<'a, 'o> {
    /// The inbox that stores and audits received messages.
    pub inbox: &'a Inbox<'a>,
    /// The inbox's store, to number a message by arrival.
    pub inbox_store: &'a dyn InboxStore,
    /// The outbox that business acknowledgements move.
    pub outbox: &'a Outbox<'o>,
    /// The outbox's store.
    pub outbox_store: &'a dyn OutboxStore,
    /// The bridge identity (the external party's pinned key).
    pub bridge: &'a dyn IdentitySigner,
    /// Where credentials are resolved, per call.
    pub vault: &'a dyn CredentialVault,
    /// How long a received message lives.
    pub lifetimes: &'a Lifetimes,
    /// The audit trail.
    pub audit: &'a dyn AuditSink,
    /// The refusal counter shared by every request for this peer (bounded: one window).
    pub refusals: &'a RefusalCounter,
}

impl<'a, 'o> ExternalReceiver<'a, 'o> {
    /// A receiver for `peer` over `ports`.
    #[must_use]
    pub fn new(peer: &'a ExternalPeer, ports: ReceiverPorts<'a, 'o>) -> Self {
        Self {
            peer,
            inbox: ports.inbox,
            inbox_store: ports.inbox_store,
            outbox: ports.outbox,
            outbox_store: ports.outbox_store,
            bridge: ports.bridge,
            vault: ports.vault,
            lifetimes: ports.lifetimes,
            audit: ports.audit,
            refusals: ports.refusals,
        }
    }

    /// Reads one inbound request for this peer, under the peer's own cap.
    ///
    /// The route contract (crate README): the edge reads the body **only** here, then hands the
    /// request to [`ExternalReceiver::handle`]. The cap is the peer's
    /// [`ExternalPeer::max_body_bytes`], never a caller's, and a peer whose cap is missing or
    /// unusable is refused rather than read without one.
    ///
    /// # Errors
    /// [`Error::InvalidPeer`] when the peer's cap is zero or does not fit the reader, so a
    /// missing cap is never an unlimited one; otherwise every error of
    /// [`InboundRequest::from_stream`].
    pub fn read<I, C>(
        &self,
        path: impl Into<String>,
        headers: Vec<(String, String)>,
        body: I,
    ) -> Result<InboundRequest>
    where
        I: IntoIterator<Item = std::io::Result<C>>,
        C: AsRef<[u8]>,
    {
        let cap = usize::try_from(self.peer.max_body_bytes)
            .ok()
            .filter(|cap| *cap > 0)
            .ok_or(Error::InvalidPeer {
                reason: "the inbound body cap is a positive number of bytes",
            })?;
        InboundRequest::from_stream(path, headers, body, cap)
    }

    /// Handles one request. Never panics and never returns an error: every outcome is an
    /// answer for the peer.
    pub async fn handle(&self, link: &Link, request: &InboundRequest, now: u64) -> InboundResponse {
        match self.try_handle(link, request, now).await {
            Ok(response) => response,
            Err(error) => InboundResponse::refused(error.status(), error.code()),
        }
    }

    async fn try_handle(
        &self,
        link: &Link,
        request: &InboundRequest,
        now: u64,
    ) -> Result<InboundResponse> {
        if request.path != self.peer.inbound.path {
            return Err(Error::UnknownPath);
        }
        // A window that ended before this request is closed first, so its summary row lands
        // even when this request is accepted.
        self.close_window(link, now)?;
        // The cap is on the bytes received, never on a header: a reader that respected the cap
        // cannot have produced a larger body, and this refuses one anyway (belt and braces, for
        // a request a test built by hand).
        if u64::try_from(request.body().len()).unwrap_or(u64::MAX) > self.peer.max_body_bytes {
            self.refuse(link, now, Error::BodyTooLarge.code())?;
            return Err(Error::BodyTooLarge);
        }
        let secret = self
            .vault
            .resolve(&self.peer.auth, micros(now))
            .await
            .map_err(|_| Error::CredentialUnavailable)?;
        let verified = crate::signature::verify_inbound(
            &self.peer.inbound.signature_scheme,
            secret.expose().as_bytes(),
            &request.headers,
            request.body(),
            now,
            self.peer.inbound.tolerance_secs,
        );
        drop(secret);
        if let Err(error) = verified {
            self.refuse(link, now, error.code())?;
            return Err(error);
        }
        if let Err(error) = self.check_link(link, now) {
            self.refuse(link, now, error.code())?;
            return Err(error);
        }
        match decode_inbound(self.peer, request.body(), now) {
            Ok(Inbound::Ack(ack)) => {
                apply_ack(
                    self.outbox,
                    self.outbox_store,
                    self.bridge,
                    self.peer,
                    link,
                    &ack,
                    now,
                )?;
                Ok(InboundResponse::ok(200, "ack_recorded"))
            }
            Ok(Inbound::Message(message)) => self.take(link, &message, now),
            Err(error) => {
                self.refuse(link, now, error.code())?;
                Err(error)
            }
        }
    }

    fn check_link(&self, link: &Link, now: u64) -> Result<()> {
        self.peer.validate_for(link)?;
        if self.bridge.party() != self.peer.party() {
            return Err(Error::PeerMismatch);
        }
        if link.state_at(now) != LinkState::Active {
            return Err(Error::LinkInactive);
        }
        Ok(())
    }

    fn take(&self, link: &Link, inbound: &InboundMessage, now: u64) -> Result<InboundResponse> {
        let id = external_message_id(link.id, &inbound.peer_id);
        let view = self
            .inbox_store
            .conversation(&link.id, &inbound.conversation)?;
        let text = serde_json::to_string(&inbound.payload).map_err(|_| Error::InvalidBody {
            reason: "the payload cannot be encoded",
        })?;
        let message = Message::sign(
            MessageParts {
                id,
                link: link.id,
                conversation: inbound.conversation.clone(),
                seq: view.next_seq.max(1),
                topic: inbound.topic.clone(),
                schema_version: inbound.schema_version,
                sent_at: now,
                expires_at: self.lifetimes.expires_at(&inbound.topic, now)?,
                tombstone: false,
                replaces: None,
                payload: canonicalize(&text)?,
            },
            self.bridge,
        )?;
        // The request was verified against this peer's credential above: it is the party of the
        // connection, whatever the body names.
        let transport = TransportParty::from_verified_connection(self.peer.party());
        let outcome = self.inbox.receive_from(link, &transport, &message, now)?;
        Ok(match outcome.status {
            ReceiveStatus::Applied | ReceiveStatus::PendingApply => {
                InboundResponse::ok(200, "accepted")
            }
            ReceiveStatus::Parked => InboundResponse::ok(202, "accepted"),
            ReceiveStatus::Duplicate => InboundResponse::ok(200, "duplicate"),
            ReceiveStatus::Busy => InboundResponse::refused(503, spot_link_exchange::codes::BUSY),
            ReceiveStatus::Rejected(code) => {
                let status = match code {
                    RejectCode::NotGranted => 403,
                    RejectCode::VersionUnsupported => 426,
                    RejectCode::OutOfOrderExpired => 410,
                    RejectCode::PayloadInvalid => 422,
                    // The link holds as much parked work as its limits allow: try again later.
                    RejectCode::CapacityExceeded => 429,
                };
                InboundResponse::refused(status, code.code())
            }
        })
    }

    fn refuse(&self, link: &Link, now: u64, code: &'static str) -> Result<()> {
        let recorded = self
            .refusals
            .record(&self.peer.id, now, self.peer.refusal_window_secs);
        if let Some(count) = recorded.closed {
            self.summarise(link, now, count)?;
        }
        if !recorded.first {
            return Ok(());
        }
        let event = LinkEvent {
            link: link.id,
            kind: LinkEventKind::Refused,
            actor: "system".to_owned(),
            at: now,
            revision: link.version,
            detail: Some(code),
        };
        let action = "record a refused delivery";
        record_event(self.audit, &event, action)
            .map_err(|_| Error::Exchange(spot_link_exchange::Error::AuditFailed { action }))
    }

    /// Closes a refusal window that ended at `now`: one summary row carrying its count.
    fn close_window(&self, link: &Link, now: u64) -> Result<()> {
        match self
            .refusals
            .close(&self.peer.id, now, self.peer.refusal_window_secs)
        {
            Some(count) => self.summarise(link, now, count),
            None => Ok(()),
        }
    }

    /// The single row a window's tail leaves behind: kind `Refused`, no detail (the window's
    /// one detail row names its first code) and a `count` payload with the refusals after that
    /// detail row. Nothing the sender wrote is in it.
    fn summarise(&self, link: &Link, now: u64, count: u64) -> Result<()> {
        let event = LinkEvent {
            link: link.id,
            kind: LinkEventKind::Refused,
            actor: "system".to_owned(),
            at: now,
            revision: link.version,
            detail: None,
        };
        let action = "record refused deliveries";
        let mut record = event
            .to_audit_record()
            .map_err(|_| Error::Exchange(spot_link_exchange::Error::AuditFailed { action }))?;
        record.payload.insert(
            "count".to_owned(),
            PayloadValue::Int(i64::try_from(count).unwrap_or(i64::MAX)),
        );
        self.audit
            .append(record)
            .map(|_| ())
            .map_err(|_| Error::Exchange(spot_link_exchange::Error::AuditFailed { action }))
    }
}

/// The buffered constructor is not reachable in a product build: an edge cannot
/// buffer a body first and check its size afterwards.
///
/// ```compile_fail
/// use spot_link_external::InboundRequest;
/// // Only `from_stream` exists with default features; `new` is test-only.
/// let _ = InboundRequest::new("/links/acme/inbound", Vec::new(), b"{}".to_vec());
/// ```
#[cfg(not(feature = "testing"))]
mod the_capped_reader_is_the_only_way_in {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::codes;

    /// One-byte chunks, `total` of them.
    fn bytes(total: usize) -> std::vec::IntoIter<std::io::Result<Vec<u8>>> {
        (0..total)
            .map(|_| Ok(vec![b'x']))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn the_reader_reserves_no_more_than_the_cap() {
        // A body at the cap fills a buffer of exactly the cap: the read never grows past it,
        // so the sender's stream cannot decide the allocation.
        let body = read_capped(bytes(8), 8).expect("a body at the cap fits");
        assert_eq!(body.len(), 8);
        assert_eq!(body.capacity(), 8);
        // And a body over the cap stops at the cap, whatever came before it.
        assert!(matches!(
            read_capped(bytes(64), 8),
            Err(Error::BodyTooLarge)
        ));
    }

    #[test]
    fn the_reader_never_buffers_more_than_the_cap_for_one_large_chunk() {
        // A single chunk far past the cap: at most the cap is copied into the buffer, and the
        // read is refused, so no request carrying it can exist.
        let chunk = vec![Ok::<_, std::io::Error>(vec![b'x'; 4_096])];
        assert!(matches!(read_capped(chunk, 8), Err(Error::BodyTooLarge)));
        // A read error is a fixed answer too: nothing from the transport reaches the peer.
        let failed = vec![Err::<Vec<u8>, _>(std::io::Error::other("socket reset"))];
        assert_eq!(
            read_capped(failed, 8).map_err(|error| error.code()),
            Err(codes::INVALID_BODY)
        );
    }

    #[test]
    fn the_buffered_constructor_builds_the_same_request_as_the_reader() {
        let buffered = InboundRequest::new("/links/acme/inbound", Vec::new(), b"hello".to_vec());
        let read = InboundRequest::from_stream(
            "/links/acme/inbound",
            Vec::new(),
            [Ok::<_, std::io::Error>(b"hello".to_vec())],
            8,
        )
        .expect("a body inside the cap is accepted");
        assert_eq!(buffered, read);
        assert_eq!(read.body(), b"hello");
    }
}
