//! The server side: the three endpoints a link's peer calls.
//!
//! * `POST /link/v1/{link}/messages`: the direct carrier. The peer pushes one signed message;
//!   `Inbox::receive` decides and the answer carries the receipts.
//! * `GET  /link/v1/{link}/mailbox`: the pull carrier. The peer long-polls for messages from this
//!   side's outbox.
//! * `POST /link/v1/{link}/acks`: the pull carrier's return path. The peer posts the signed
//!   receipts for what it took from the mailbox.
//!
//! # The order of checks
//!
//! Every request passes the same gate, in this order, before any body is read:
//!
//! 1. a client certificate must be present (`401`);
//! 2. the certificate must be pinned to a party and not revoked (`403 forbidden`);
//! 3. the link must exist and the party must be **its peer**; an unknown link and somebody else's
//!    link answer identically (`403 forbidden`), so the endpoint does not reveal which links
//!    exist. Only here is the caller tied to the link;
//! 4. the link must be `Active`: a revoked link answers `403 link_transport.revoked`, any other
//!    state `403 link_transport.not_active`, and each is audited;
//! 5. this side must use this carrier in this direction (`403 carrier_disabled`);
//! 6. the link's rate limit, then its concurrency bound, are charged (`429`, `503`).
//!
//! The rate limit, the concurrency gate and the refusal audit are keyed by things that exist
//! only after step 3 (a link the caller is the peer of, an enrolled party), so a caller without a
//! pinned certificate cannot create state, spend a link's budget or write an audit record.
//!
//! The party from step 2 goes into `Inbox::receive` and `Outbox::on_receipt` as the transport
//! peer; nothing read from a body is ever used to decide who the caller is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{Extensions, header};
use axum::routing::{get, post};
use serde::Deserialize;
use spot_link_exchange::{ReceiveStatus, TransportParty, codes as exchange_codes};
use spot_sync_http::{ClientCertificate, Sleeper, TokioSleeper};
use spot_tenant_link::{
    Link, LinkEvent, LinkEventKind, LinkId, LinkState, Party, codes as link_codes, record_event,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::carrier::{CarrierTable, LinkCarriers};
use crate::error::{Error, Result};
use crate::host::{LinkHost, blocking};
use crate::identity::PeerCertificates;
use crate::intake::{IntakeCounters, IntakeRefusal, IntakeSettings, LinkIntake};
use crate::limits::{RateConfig, RateLimiter};
use crate::wire::{
    AckReply, AckRequest, AckResult, IntakeReply, MailboxReply, ReceiveReply, WireEffect,
    WireStatus,
};

/// Limits and timings of the server.
#[derive(Clone, Copy, Debug)]
pub struct ServerSettings {
    /// The largest `POST /messages` body (a message carries at most 256 KiB of payload).
    pub max_message_bytes: usize,
    /// The largest `POST /acks` body.
    pub max_ack_bytes: usize,
    /// The most receipts one `POST /acks` may carry.
    pub max_receipts_per_ack: usize,
    /// The most messages one mailbox answer may carry.
    pub max_batch_messages: usize,
    /// The longest a mailbox request waits for something to send.
    pub long_poll: Duration,
    /// How often a waiting mailbox request looks again (and re-checks that the link is active).
    pub poll_interval: Duration,
    /// The longest the server waits for a request body once the headers are in.
    pub body_timeout: Duration,
    /// The rate limit of one link.
    pub link_rate: RateConfig,
    /// The budget of refusals that may be audited per enrolled party.
    pub refusal_rate: RateConfig,
    /// The most requests in flight on one link (a waiting mailbox request counts).
    pub max_concurrent_per_link: usize,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            max_message_bytes: 512 * 1024,
            max_ack_bytes: 1024 * 1024,
            max_receipts_per_ack: 256,
            max_batch_messages: 16,
            long_poll: Duration::from_secs(25),
            poll_interval: Duration::from_secs(1),
            body_timeout: Duration::from_secs(10),
            link_rate: RateConfig::default(),
            refusal_rate: RateConfig {
                burst: 10,
                per_second: 1,
                max_keys: 10_000,
            },
            max_concurrent_per_link: 8,
        }
    }
}

/// A link server: assemble it, then mount [`LinkServer::router`] on a listener that puts the
/// peer certificate in each request (`spot_sync_http::tls::serve`, or the loopback transport).
#[derive(Clone)]
pub struct LinkServer {
    host: LinkHost,
    certificates: Arc<dyn PeerCertificates>,
    carriers: Arc<CarrierTable>,
    settings: ServerSettings,
    sleeper: Arc<dyn Sleeper>,
    intake: Option<(Arc<dyn LinkIntake>, IntakeSettings)>,
    counters: Arc<IntakeCounters>,
}

impl std::fmt::Debug for LinkServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkServer")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl LinkServer {
    /// A server over `host`, authenticating callers through `certificates`.
    #[must_use]
    pub fn new(
        host: LinkHost,
        certificates: Arc<dyn PeerCertificates>,
        carriers: Arc<CarrierTable>,
    ) -> Self {
        Self {
            host,
            certificates,
            carriers,
            settings: ServerSettings::default(),
            sleeper: Arc::new(TokioSleeper),
            intake: None,
            counters: Arc::new(IntakeCounters::default()),
        }
    }

    /// Mounts the host intake routes (`POST /link/v1/handshake`, `POST /link/v1/{link}/revoke`)
    /// over `intake`. Without this call the routes do not exist.
    #[must_use]
    pub fn with_intake(mut self, intake: Arc<dyn LinkIntake>, settings: IntakeSettings) -> Self {
        self.intake = Some((intake, settings));
        self
    }

    /// The counters of the intake routes (shared by every router this server builds).
    #[must_use]
    pub fn intake_counters(&self) -> Arc<IntakeCounters> {
        Arc::clone(&self.counters)
    }

    /// Replaces the limits and timings.
    #[must_use]
    pub fn with_settings(mut self, settings: ServerSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Replaces the sleeper the long-poll waits with.
    #[must_use]
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// The router. Each call builds fresh limiters, so mount one router per listener.
    pub fn router(&self) -> Router {
        let shared = Arc::new(Shared {
            host: self.host.clone(),
            certificates: Arc::clone(&self.certificates),
            carriers: Arc::clone(&self.carriers),
            settings: self.settings,
            sleeper: Arc::clone(&self.sleeper),
            links: RateLimiter::new(self.settings.link_rate),
            refusals: RateLimiter::new(self.settings.refusal_rate),
            gates: Mutex::new(HashMap::new()),
            intake: self.intake.as_ref().map(|(port, settings)| Intake {
                port: Arc::clone(port),
                settings: *settings,
                peers: RateLimiter::new(settings.rate),
                counters: Arc::clone(&self.counters),
            }),
        });
        let router = Router::new()
            .route("/link/v1/{link}/messages", post(messages))
            .route("/link/v1/{link}/mailbox", get(mailbox))
            .route("/link/v1/{link}/acks", post(acks));
        let router = if self.intake.is_some() {
            router
                .route("/link/v1/handshake", post(handshake))
                .route("/link/v1/{link}/revoke", post(revoke))
        } else {
            router
        };
        router.with_state(shared)
    }
}

struct Shared {
    host: LinkHost,
    certificates: Arc<dyn PeerCertificates>,
    carriers: Arc<CarrierTable>,
    settings: ServerSettings,
    sleeper: Arc<dyn Sleeper>,
    links: RateLimiter,
    refusals: RateLimiter,
    gates: Mutex<HashMap<LinkId, Arc<Semaphore>>>,
    intake: Option<Intake>,
}

/// The intake routes' port, limits and second-tier limiter.
struct Intake {
    port: Arc<dyn LinkIntake>,
    settings: IntakeSettings,
    peers: RateLimiter,
    counters: Arc<IntakeCounters>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    Messages,
    Mailbox,
    Acks,
}

/// A request that passed the gate.
struct Session {
    link: Link,
    peer: Party,
    now: u64,
    _permit: OwnedSemaphorePermit,
}

impl Shared {
    fn monotonic(&self) -> Duration {
        self.host.clock.monotonic()
    }

    /// The gate (see the module documentation for the order).
    async fn authorize(
        &self,
        certificate: Option<ClientCertificate>,
        link_text: &str,
        endpoint: Endpoint,
    ) -> Result<Session> {
        let certificate = certificate.ok_or_else(Error::identity_required)?;
        let peer = self
            .certificates
            .party(&certificate)
            .ok_or_else(Error::forbidden)?;
        let now = self.host.now_ms()?;
        let id: LinkId = link_text.parse().map_err(|_| Error::forbidden())?;
        let link = self.load(id).await?.ok_or_else(Error::forbidden)?;
        if link.peer() != Some(&peer) {
            return Err(self
                .refuse(
                    &peer,
                    &link,
                    link_codes::WRONG_LINK,
                    now,
                    Error::forbidden(),
                )
                .await);
        }
        match link.state_at(now) {
            LinkState::Active => {}
            LinkState::Revoked => {
                return Err(self
                    .refuse(&peer, &link, link_codes::REVOKED, now, Error::revoked())
                    .await);
            }
            LinkState::Expired => {
                return Err(self
                    .refuse(
                        &peer,
                        &link,
                        link_codes::LINK_EXPIRED,
                        now,
                        Error::not_active(),
                    )
                    .await);
            }
            LinkState::Proposed | LinkState::AcceptedByPeer => {
                return Err(self
                    .refuse(
                        &peer,
                        &link,
                        link_codes::NOT_ACTIVE,
                        now,
                        Error::not_active(),
                    )
                    .await);
            }
        }
        let carriers: LinkCarriers = self.carriers.of(&link);
        let enabled = match endpoint {
            Endpoint::Messages => carriers.accepts_pushes(),
            Endpoint::Mailbox | Endpoint::Acks => carriers.serves_mailbox(),
        };
        if !enabled {
            return Err(Error::carrier_disabled());
        }
        self.links
            .take(&link.id.to_string(), self.monotonic())
            .map_err(Error::rate_limited)?;
        let permit = self.gate(link.id)?;
        Ok(Session {
            link,
            peer,
            now,
            _permit: permit,
        })
    }

    async fn load(&self, id: LinkId) -> Result<Option<Link>> {
        let store = Arc::clone(&self.host.links);
        match blocking(move || store.get(&id)).await? {
            Ok(link) => Ok(Some(link)),
            Err(spot_tenant_link::Error::NotFound { .. }) => Ok(None),
            Err(_) => Err(Error::unavailable()),
        }
    }

    fn gate(&self, id: LinkId) -> Result<OwnedSemaphorePermit> {
        let gate = {
            let mut gates = self
                .gates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(gates.entry(id).or_insert_with(|| {
                Arc::new(Semaphore::new(self.settings.max_concurrent_per_link.max(1)))
            }))
        };
        gate.try_acquire_owned().map_err(|_| Error::busy())
    }

    /// Audits a refusal for an enrolled party on an existing link, inside the party's refusal
    /// budget, and returns `error`. A failing audit does not turn a refusal into an acceptance.
    async fn refuse(
        &self,
        peer: &Party,
        link: &Link,
        detail: &'static str,
        now: u64,
        error: Error,
    ) -> Error {
        if self
            .refusals
            .take(&peer.canonical(), self.monotonic())
            .is_ok()
        {
            let audit = Arc::clone(&self.host.audit);
            let event = LinkEvent {
                link: link.id,
                kind: LinkEventKind::Refused,
                actor: "system".to_owned(),
                at: now,
                revision: link.version,
                detail: Some(detail),
            };
            let written =
                blocking(move || record_event(audit.as_ref(), &event, "record a refused request"))
                    .await;
            if !matches!(written, Ok(Ok(()))) {
                tracing::warn!(
                    code = error.code,
                    "a refused link request could not be audited"
                );
            }
        }
        error
    }

    /// Reads a body of at most `limit` bytes within the body timeout.
    async fn read_body(&self, request: Request, limit: usize) -> Result<axum::body::Bytes> {
        let declared = request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.parse::<usize>().ok());
        if declared.is_some_and(|length| length > limit) {
            return Err(Error::too_large());
        }
        let read = axum::body::to_bytes(request.into_body(), limit);
        match tokio::time::timeout(self.settings.body_timeout, read).await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(_)) => Err(Error::too_large()),
            Err(_) => Err(Error::invalid_request("the request body was too slow")),
        }
    }
}

impl Shared {
    /// The intake gate: certificate (`401`), pinned party (`403`), then the second-tier limiter
    /// (`429`). Nothing is read, verified, claimed or audited before the limiter has spoken, and
    /// a refused call leaves only the aggregated counter.
    fn intake_gate(&self, extensions: &Extensions) -> Result<(&Intake, Party)> {
        let intake = self.intake.as_ref().ok_or_else(Error::forbidden)?;
        let certificate = certificate_of(extensions).ok_or_else(Error::identity_required)?;
        let peer = self
            .certificates
            .party(&certificate)
            .ok_or_else(Error::forbidden)?;
        if let Err(wait) = intake.peers.take(&peer.canonical(), self.monotonic()) {
            intake.counters.count_limited();
            return Err(Error::rate_limited(wait));
        }
        Ok((intake, peer))
    }
}

/// Maps the port's refusal to what the peer may be told.
fn intake_error(refusal: IntakeRefusal) -> Error {
    match refusal {
        IntakeRefusal::Forged => Error::forbidden(),
        IntakeRefusal::Replayed => Error::replayed(),
        IntakeRefusal::Stale => Error::conflict(),
        IntakeRefusal::Unavailable => Error::unavailable(),
    }
}

async fn handshake(
    State(shared): State<Arc<Shared>>,
    request: Request,
) -> Result<Json<IntakeReply>> {
    let (intake, peer) = shared.intake_gate(request.extensions())?;
    let limit = intake.settings.max_handshake_bytes;
    let port = Arc::clone(&intake.port);
    let body = shared.read_body(request, limit).await?;
    let message: spot_tenant_link::HandshakeMessage = serde_json::from_slice(&body)
        .map_err(|_| Error::invalid_request("the body is not a handshake message"))?;
    let now = shared.host.now_ms()?;
    // The message must belong to the caller. A proposal names its proposer; a later message
    // belongs to a link the caller is the peer of. Unknown and foreign answer the same.
    match &message.body {
        spot_tenant_link::HandshakeBody::Propose(proposal) => {
            if proposal.draft.local != peer {
                return Err(Error::forbidden());
            }
        }
        other => {
            let id = other.link().copied().ok_or_else(Error::forbidden)?;
            let link = shared.load(id).await?.ok_or_else(Error::forbidden)?;
            if link.peer() != Some(&peer) {
                return Err(Error::forbidden());
            }
        }
    }
    blocking(move || port.handshake(&peer, &message, now))
        .await?
        .map_err(intake_error)?;
    Ok(Json(IntakeReply { accepted: true }))
}

async fn revoke(
    State(shared): State<Arc<Shared>>,
    Path(link_text): Path<String>,
    request: Request,
) -> Result<Json<IntakeReply>> {
    let (intake, peer) = shared.intake_gate(request.extensions())?;
    let limit = intake.settings.max_revoke_bytes;
    let port = Arc::clone(&intake.port);
    let body = shared.read_body(request, limit).await?;
    let message: spot_tenant_link::RevokeMessage = serde_json::from_slice(&body)
        .map_err(|_| Error::invalid_request("the body is not a revoke message"))?;
    let id: LinkId = link_text.parse().map_err(|_| Error::forbidden())?;
    if message.link != id {
        return Err(Error::forbidden());
    }
    let link = shared.load(id).await?.ok_or_else(Error::forbidden)?;
    if link.peer() != Some(&peer) {
        return Err(Error::forbidden());
    }
    let now = shared.host.now_ms()?;
    blocking(move || port.revoke(&peer, &message, now))
        .await?
        .map_err(intake_error)?;
    Ok(Json(IntakeReply { accepted: true }))
}

fn certificate_of(extensions: &Extensions) -> Option<ClientCertificate> {
    extensions.get::<ClientCertificate>().cloned()
}

/// Maps an exchange failure to what the peer may be told.
fn exchange_error(error: &spot_link_exchange::Error) -> Error {
    match error.code() {
        exchange_codes::IDENTITY_MISMATCH
        | exchange_codes::LINK_MISMATCH
        | exchange_codes::UNKNOWN_KEY
        | exchange_codes::BAD_SIGNATURE => Error::forbidden(),
        exchange_codes::BUSY => Error::busy(),
        _ => Error::unavailable(),
    }
}

async fn messages(
    State(shared): State<Arc<Shared>>,
    Path(link_text): Path<String>,
    request: Request,
) -> Result<Json<ReceiveReply>> {
    let session = shared
        .authorize(
            certificate_of(request.extensions()),
            &link_text,
            Endpoint::Messages,
        )
        .await?;
    let body = shared
        .read_body(request, shared.settings.max_message_bytes)
        .await?;
    let message: spot_link_exchange::Message = serde_json::from_slice(&body)
        .map_err(|_| Error::invalid_request("the body is not a link message"))?;
    let host = shared.host.clone();
    let Session {
        link, peer, now, ..
    } = &session;
    // The session's peer is the party the client certificate was verified against.
    let (link, peer, now) = (
        link.clone(),
        TransportParty::from_verified_connection(peer.clone()),
        *now,
    );
    let outcome = blocking(move || host.inbox().receive_from(&link, &peer, &message, now))
        .await?
        .map_err(|error| exchange_error(&error))?;
    let status = WireStatus::of(outcome.status).ok_or_else(Error::busy)?;
    debug_assert!(!matches!(outcome.status, ReceiveStatus::Busy));
    Ok(Json(ReceiveReply {
        status,
        receipts: outcome.receipts,
    }))
}

#[derive(Deserialize)]
struct MailboxQuery {
    limit: Option<usize>,
    wait_ms: Option<u64>,
}

async fn mailbox(
    State(shared): State<Arc<Shared>>,
    Path(link_text): Path<String>,
    request: Request,
) -> Result<Json<MailboxReply>> {
    let session = shared
        .authorize(
            certificate_of(request.extensions()),
            &link_text,
            Endpoint::Mailbox,
        )
        .await?;
    let query = Query::<MailboxQuery>::try_from_uri(request.uri())
        .map_err(|_| Error::invalid_request("the mailbox query is not valid"))?
        .0;
    let settings = shared.settings;
    let limit = query
        .limit
        .unwrap_or(settings.max_batch_messages)
        .clamp(1, settings.max_batch_messages.max(1));
    let wait = Duration::from_millis(query.wait_ms.unwrap_or(0)).min(settings.long_poll);
    let started = shared.monotonic();
    let interval = settings.poll_interval.max(Duration::from_millis(1));
    let polls = wait.as_millis() / interval.as_millis() + 1;
    let mut polled: u128 = 0;
    loop {
        // The link is read again on every pass: a revocation during the wait ends the wait with
        // `revoked` and releases nothing.
        let now = shared.host.now_ms()?;
        let link = shared
            .load(session.link.id)
            .await?
            .filter(|link| link.peer() == Some(&session.peer))
            .ok_or_else(Error::forbidden)?;
        match link.state_at(now) {
            LinkState::Active => {}
            LinkState::Revoked => {
                return Err(shared
                    .refuse(
                        &session.peer,
                        &link,
                        link_codes::REVOKED,
                        now,
                        Error::revoked(),
                    )
                    .await);
            }
            _ => {
                return Err(shared
                    .refuse(
                        &session.peer,
                        &link,
                        link_codes::NOT_ACTIVE,
                        now,
                        Error::not_active(),
                    )
                    .await);
            }
        }
        let host = shared.host.clone();
        let batch = blocking(move || host.outbox().next_batch(&link, now, limit, "system", None))
            .await?
            .map_err(|error| exchange_error(&error))?;
        polled += 1;
        let waited = shared.monotonic().saturating_sub(started);
        if !batch.is_empty() || waited >= wait || polled >= polls {
            return Ok(Json(MailboxReply { messages: batch }));
        }
        shared
            .sleeper
            .sleep(interval.min(wait.saturating_sub(waited)))
            .await;
    }
}

async fn acks(
    State(shared): State<Arc<Shared>>,
    Path(link_text): Path<String>,
    request: Request,
) -> Result<Json<AckReply>> {
    let session = shared
        .authorize(
            certificate_of(request.extensions()),
            &link_text,
            Endpoint::Acks,
        )
        .await?;
    let body = shared
        .read_body(request, shared.settings.max_ack_bytes)
        .await?;
    let parsed: AckRequest = serde_json::from_slice(&body)
        .map_err(|_| Error::invalid_request("the body is not a list of receipts"))?;
    if parsed.receipts.len() > shared.settings.max_receipts_per_ack {
        return Err(Error::too_large());
    }
    let host = shared.host.clone();
    let Session {
        link, peer, now, ..
    } = &session;
    let (link, peer, now) = (
        link.clone(),
        TransportParty::from_verified_connection(peer.clone()),
        *now,
    );
    let results = blocking(move || -> std::result::Result<Vec<AckResult>, Error> {
        let outbox = host.outbox();
        let mut results = Vec::with_capacity(parsed.receipts.len());
        for receipt in &parsed.receipts {
            let effect = match outbox.on_receipt_from(&link, &peer, receipt, now) {
                Ok(effect) => WireEffect::of(effect),
                Err(spot_link_exchange::Error::NotFound) => WireEffect::Unknown,
                Err(error) => return Err(exchange_error(&error)),
            };
            results.push(AckResult {
                message: receipt.message,
                effect,
            });
        }
        Ok(results)
    })
    .await??;
    Ok(Json(AckReply { results }))
}
