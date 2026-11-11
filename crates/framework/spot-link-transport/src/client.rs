//! The client side: `push_loop` (the direct carrier) and `pull_loop` (the pull carrier).
//!
//! Each loop is a thin repetition of a *step* that does one bounded unit of work and can be
//! driven by hand in a test:
//!
//! * [`LinkClient::push_step`] takes the next batch from the outbox, posts each message to the
//!   peer's `/messages` in order, and applies the signed receipts in the answer.
//! * [`LinkClient::pull_step`] long-polls the peer's `/mailbox`, gives each message to
//!   `Inbox::receive_from`, and posts the resulting receipts to the peer's `/acks`.
//!
//! A step that fails part-way leaves the outbox/inbox exactly as the exchange left it: a message
//! that was handed over but not confirmed is handed over again after the outbox's resend delay,
//! and the receiver answers the repeat with the earlier outcome. That is the whole recovery
//! story, and it is why there is no transport cursor.
//!
//! Every network call runs under a deadline, bounded retries with injected jitter, the link's
//! circuit breaker and the shared bulkhead. A peer that stays down opens the breaker; the loop
//! then probes once per breaker period instead of hammering it, and [`LinkClient::conditions`]
//! raises `LINK_PEER_UNREACHABLE` next to the stuck-message conditions.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_link_exchange::{Thresholds, TransportParty, stuck_messages};
use spot_quarantine::{Condition, Severity};
use spot_sync_http::{
    BreakerConfig, CircuitBreaker, Deadline, HttpRequest, HttpResponse, JitterSource, Permit,
    RetryPolicy, Sleeper,
};
use spot_tenant_link::{Carrier, Link, LinkId, LinkState};
use tokio::sync::{Semaphore, watch};

use crate::carrier::LinkCarriers;
use crate::error::{Error, ErrorBody, Result, from_remote};
use crate::host::{LinkHost, blocking};
use crate::identity::PeerTransport;
use crate::wire::{AckRequest, CONTENT_TYPE, MailboxReply, ReceiveReply};

/// Client limits and timings.
#[derive(Clone, Copy, Debug)]
pub struct ClientConfig {
    /// The time one call (with its retries) may take, not counting a long-poll's wait.
    pub deadline: Duration,
    /// Retries within one call.
    pub retry: RetryPolicy,
    /// The per-link breaker. Its `open_for` is the probing period for a peer that stays down.
    pub breaker: BreakerConfig,
    /// The most messages one push step takes from the outbox.
    pub push_batch: usize,
    /// The most messages one pull step asks for.
    pub pull_batch: usize,
    /// How long a pull step asks the peer to wait for something to send.
    pub pull_wait: Duration,
    /// How long the push loop pauses when there was nothing to send.
    pub idle_wait: Duration,
    /// How long the peer may stay unreachable before it is reported as a condition.
    pub unreachable_after_ms: u64,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(30),
            retry: RetryPolicy {
                max_attempts: 3,
                base: Duration::from_millis(200),
                factor: 2,
                cap: Duration::from_secs(300),
            },
            breaker: BreakerConfig {
                failure_threshold: 5,
                open_for: Duration::from_secs(300),
                half_open_probes: 1,
            },
            push_batch: 16,
            pull_batch: 16,
            pull_wait: Duration::from_secs(20),
            idle_wait: Duration::from_secs(5),
            unreachable_after_ms: 15 * 60_000,
        }
    }
}

/// The injected pieces of the ADR-072 kit a client needs.
pub struct Resilience {
    /// Jitter for backoffs (injected, never ambient).
    pub jitter: Box<dyn JitterSource>,
    /// Where waits happen.
    pub sleeper: Arc<dyn Sleeper>,
    /// Bounds the calls in flight; share one across the clients of all links to bound the whole
    /// process.
    pub bulkhead: Arc<Semaphore>,
}

/// What one step did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepReport {
    /// Messages handed to the peer (push) or taken from its mailbox (pull).
    pub moved: usize,
    /// Receipts applied (push) or posted (pull).
    pub receipts: usize,
    /// Messages the exchange refused as not attributable to the peer (pull).
    pub refused: usize,
    /// Whether the peer answered at all.
    pub contacted: bool,
}

/// How the peer has been answering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Health {
    /// Calls that failed in a row because the peer was unreachable or too slow.
    pub consecutive_failures: u32,
    /// Unix milliseconds the current run of failures began.
    pub failing_since: Option<u64>,
    /// Unix milliseconds of the last answer.
    pub last_success: Option<u64>,
    /// The code of the last failure.
    pub last_error: Option<&'static str>,
}

/// One link's client.
pub struct LinkClient {
    host: LinkHost,
    link: LinkId,
    peer: PeerTransport,
    carriers: LinkCarriers,
    config: ClientConfig,
    breaker: CircuitBreaker,
    jitter: Mutex<Box<dyn JitterSource>>,
    sleeper: Arc<dyn Sleeper>,
    bulkhead: Arc<Semaphore>,
    health: Mutex<Health>,
}

impl std::fmt::Debug for LinkClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkClient")
            .field("link", &self.link)
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

impl LinkClient {
    /// A client for `link` over `peer`, with `carriers` for the two directions.
    ///
    ///
    /// # Errors
    /// A breaker configuration with a zero failure threshold.
    pub fn new(
        host: LinkHost,
        link: LinkId,
        peer: PeerTransport,
        carriers: LinkCarriers,
        config: ClientConfig,
        resilience: Resilience,
    ) -> Result<Self> {
        let breaker = CircuitBreaker::new(config.breaker).map_err(|_| {
            Error::invalid_request("the breaker needs a failure threshold of at least one")
        })?;
        Ok(Self {
            host,
            link,
            peer,
            carriers,
            config,
            breaker,
            jitter: Mutex::new(resilience.jitter),
            sleeper: resilience.sleeper,
            bulkhead: resilience.bulkhead,
            health: Mutex::new(Health::default()),
        })
    }

    /// How the peer has been answering.
    #[must_use]
    pub fn health(&self) -> Health {
        *self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The conditions of this link at `now`: the stuck-message conditions of the exchange, plus
    /// `LINK_PEER_UNREACHABLE` when the peer has not answered for `unreachable_after_ms`.
    ///
    /// # Errors
    /// [`Error::unavailable`] when a store fails.
    pub fn conditions(&self, now: u64, thresholds: &Thresholds) -> Result<Vec<Condition>> {
        let link = self
            .host
            .links
            .get(&self.link)
            .map_err(|_| Error::unavailable())?;
        let mut out = stuck_messages(
            self.host.outbox.as_ref(),
            self.host.inbox.as_ref(),
            std::slice::from_ref(&link),
            now,
            thresholds,
        )
        .map_err(|_| Error::unavailable())?;
        let health = self.health();
        if let Some(since) = health.failing_since {
            let down = now.saturating_sub(since);
            if down >= self.config.unreachable_after_ms {
                let peer = self.peer.peer();
                out.push(Condition {
                    code: "LINK_PEER_UNREACHABLE".to_owned(),
                    severity: if down >= 24 * 3_600_000 {
                        Severity::Red
                    } else {
                        Severity::Amber
                    },
                    subject: format!("link {}", self.link),
                    sentence: format!(
                        "{peer} has not answered for {}; messages wait in the queue and the link is probed from time to time.",
                        plain(down),
                    ),
                    action: "Check that the peer's server is running and reachable; nothing is lost while it is down.".to_owned(),
                });
            }
        }
        Ok(out)
    }

    /// One push step (the direct carrier).
    ///
    /// # Errors
    /// [`Error::carrier_disabled`] when this side does not send directly on this link, a local
    /// refusal ([`Error::revoked`], [`Error::not_active`], [`Error::forbidden`]) before anything
    /// is sent, or the first failure of a call to the peer (the rest of the batch stays in the
    /// outbox and is handed over again after the resend delay).
    pub async fn push_step(&self) -> Result<StepReport> {
        if self.carriers.send != Carrier::Direct {
            return Err(Error::carrier_disabled());
        }
        let (link, now) = self.active_link().await?;
        let first = self.begin()?;
        let host = self.host.clone();
        let batch_link = link.clone();
        let limit = self.config.push_batch.max(1);
        let batch = blocking(move || {
            host.outbox()
                .next_batch(&batch_link, now, limit, "system", None)
        })
        .await?
        .map_err(|_| Error::unavailable())?;
        let mut report = StepReport::default();
        let mut permit = Some(first);
        for message in batch {
            let body = serde_json::to_vec(&message).map_err(|_| Error::unavailable())?;
            let request = HttpRequest::post(format!("/link/v1/{}/messages", self.link), body)
                .with_content_type(CONTENT_TYPE);
            let outcome = self
                .call(request, self.config.deadline, permit.take())
                .await;
            let reply: ReceiveReply = self.settle(outcome)?;
            report.contacted = true;
            report.moved += 1;
            let applied = self.apply_receipts(&link, reply.receipts).await?;
            report.receipts += applied;
        }
        Ok(report)
    }

    async fn apply_receipts(
        &self,
        link: &Link,
        receipts: Vec<spot_link_exchange::Receipt>,
    ) -> Result<usize> {
        let host = self.host.clone();
        let link = link.clone();
        // The party the pinned fingerprint of this connection names, not one read from a receipt.
        let peer = TransportParty::from_verified_connection(self.peer.peer().clone());
        let now = self.host.now_ms()?;
        blocking(move || -> Result<usize> {
            let outbox = host.outbox();
            let mut applied = 0;
            for receipt in &receipts {
                match outbox.on_receipt_from(&link, &peer, receipt, now) {
                    Ok(_) => applied += 1,
                    Err(spot_link_exchange::Error::NotFound) => {}
                    Err(error) => return Err(receipt_error(&error)),
                }
            }
            Ok(applied)
        })
        .await?
    }

    /// One pull step (the pull carrier): ask the peer's mailbox, receive, acknowledge.
    ///
    /// # Errors
    /// As [`LinkClient::push_step`], for the receiving direction.
    pub async fn pull_step(&self) -> Result<StepReport> {
        if self.carriers.receive != Carrier::Pull {
            return Err(Error::carrier_disabled());
        }
        let (link, _) = self.active_link().await?;
        let first = self.begin()?;
        let wait = self.config.pull_wait;
        let request =
            HttpRequest::get(format!("/link/v1/{}/mailbox", self.link)).with_query(format!(
                "limit={}&wait_ms={}",
                self.config.pull_batch.max(1),
                wait.as_millis()
            ));
        let outcome = self
            .call(
                request,
                self.config.deadline.saturating_add(wait),
                Some(first),
            )
            .await;
        let reply: MailboxReply = self.settle(outcome)?;
        let mut report = StepReport {
            contacted: true,
            ..StepReport::default()
        };
        if reply.messages.is_empty() {
            return Ok(report);
        }
        report.moved = reply.messages.len();
        let host = self.host.clone();
        let received_link = link.clone();
        // The party the pinned fingerprint of this connection names, not one read from a message.
        let peer = TransportParty::from_verified_connection(self.peer.peer().clone());
        let now = self.host.now_ms()?;
        let taken = blocking(move || {
            let inbox = host.inbox();
            let mut taken = Taken::default();
            for message in &reply.messages {
                match inbox.receive_from(&received_link, &peer, message, now) {
                    Ok(outcome) => taken.receipts.extend(outcome.receipts),
                    Err(error) if is_unattributable(&error) => taken.refused += 1,
                    Err(_) => {
                        taken.failed = true;
                        break;
                    }
                }
            }
            taken
        })
        .await?;
        report.refused = taken.refused;
        report.receipts = taken.receipts.len();
        if !taken.receipts.is_empty() {
            let body = serde_json::to_vec(&AckRequest {
                receipts: taken.receipts,
            })
            .map_err(|_| Error::unavailable())?;
            let request = HttpRequest::post(format!("/link/v1/{}/acks", self.link), body)
                .with_content_type(CONTENT_TYPE);
            let outcome = self.call(request, self.config.deadline, None).await;
            let _: crate::wire::AckReply = self.settle(outcome)?;
        }
        if taken.failed {
            return Err(Error::unavailable());
        }
        Ok(report)
    }

    /// Runs push steps until `shutdown` is signalled (or its sender is dropped).
    pub async fn push_loop(&self, shutdown: watch::Receiver<bool>) {
        self.run(shutdown, true).await;
    }

    /// Runs pull steps until `shutdown` is signalled (or its sender is dropped).
    pub async fn pull_loop(&self, shutdown: watch::Receiver<bool>) {
        self.run(shutdown, false).await;
    }

    async fn run(&self, mut shutdown: watch::Receiver<bool>, push: bool) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            let step = if push {
                self.push_step().await
            } else {
                self.pull_step().await
            };
            let pause = match step {
                Ok(report) if report.moved > 0 => Duration::ZERO,
                Ok(_) if push => self.config.idle_wait,
                Ok(_) => Duration::ZERO,
                Err(error) => self.failure_pause(&error),
            };
            if pause.is_zero() {
                tokio::task::yield_now().await;
                continue;
            }
            tokio::select! {
                () = self.sleeper.sleep(pause) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// How long a loop waits after `error`: an unreachable peer backs off exponentially with
    /// jitter up to the retry cap and then probes at the breaker period; a local refusal
    /// (revoked, not active, carrier off) is retried at the cap, so it neither spins nor waits
    /// forever.
    fn failure_pause(&self, error: &Error) -> Duration {
        let retry = self.config.retry;
        let failures = self.health().consecutive_failures;
        let jitter = self
            .jitter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .permille();
        let backoff = retry.delay(failures.saturating_add(1), jitter);
        match error.code {
            crate::error::codes::CIRCUIT_OPEN => {
                backoff.max(error.retry_after.unwrap_or(Duration::ZERO))
            }
            crate::error::codes::PEER_UNAVAILABLE | crate::error::codes::DEADLINE_EXCEEDED => {
                backoff.max(retry.base)
            }
            _ => retry.cap,
        }
    }

    /// The link as the local store holds it, checked to be active and ours.
    async fn active_link(&self) -> Result<(Link, u64)> {
        let now = self.host.now_ms()?;
        let store = Arc::clone(&self.host.links);
        let id = self.link;
        let link = blocking(move || store.get(&id))
            .await?
            .map_err(|_| Error::unavailable())?;
        if link.peer() != Some(self.peer.peer()) {
            return Err(Error::forbidden());
        }
        match link.state_at(now) {
            LinkState::Active => Ok((link, now)),
            LinkState::Revoked => Err(Error::revoked()),
            _ => Err(Error::not_active()),
        }
    }

    fn begin(&self) -> Result<Permit> {
        self.breaker
            .acquire(&self.destination(), self.host.clock.monotonic())
            .map_err(|_| Error::circuit_open(self.config.breaker.open_for))
    }

    fn destination(&self) -> String {
        format!("link:{}", self.link)
    }

    /// One request under the deadline, the bulkhead, the breaker and the retry policy.
    async fn call(
        &self,
        request: HttpRequest,
        budget: Duration,
        mut first: Option<Permit>,
    ) -> Result<HttpResponse> {
        let deadline = Deadline::new(self.host.clock.monotonic(), budget);
        let mut attempt: u32 = 1;
        loop {
            let now = self.host.clock.monotonic();
            if deadline.is_expired(now) {
                return Err(Error::deadline_exceeded());
            }
            let permit = match first.take() {
                Some(permit) => permit,
                None => self
                    .breaker
                    .acquire(&self.destination(), now)
                    .map_err(|_| Error::circuit_open(self.config.breaker.open_for))?,
            };
            let remaining = deadline.remaining(now);
            let slot = tokio::time::timeout(remaining, Arc::clone(&self.bulkhead).acquire_owned())
                .await
                .map_err(|_| Error::deadline_exceeded())?
                .map_err(|_| Error::unavailable())?;
            let sent =
                tokio::time::timeout(remaining, self.peer.transport().request(request.clone()))
                    .await;
            drop(slot);
            match sent {
                Err(_) => {
                    permit.record_failure(now);
                    self.note_failure(crate::error::codes::DEADLINE_EXCEEDED);
                    return Err(Error::deadline_exceeded());
                }
                Ok(Ok(response)) if response.status == 429 => permit.record_success(),
                Ok(Ok(response)) if response.status < 500 => {
                    permit.record_success();
                    self.note_success();
                    return Ok(response);
                }
                Ok(Ok(_) | Err(_)) => permit.record_failure(now),
            }
            if !self.config.retry.allows(attempt.saturating_add(1)) {
                self.note_failure(crate::error::codes::PEER_UNAVAILABLE);
                return Err(Error::peer_unavailable());
            }
            let jitter = self
                .jitter
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .permille();
            let delay = self.config.retry.delay(attempt.saturating_add(1), jitter);
            if deadline.remaining(self.host.clock.monotonic()) <= delay {
                self.note_failure(crate::error::codes::DEADLINE_EXCEEDED);
                return Err(Error::deadline_exceeded());
            }
            self.sleeper.sleep(delay).await;
            attempt = attempt.saturating_add(1);
        }
    }

    /// Turns a response into its typed body, or into the error the peer's answer means.
    fn settle<T: serde::de::DeserializeOwned>(&self, outcome: Result<HttpResponse>) -> Result<T> {
        let response = outcome?;
        if response.is_success() {
            return serde_json::from_slice(&response.body).map_err(|_| Error::peer_misbehaved());
        }
        let code = serde_json::from_slice::<ErrorBody>(&response.body)
            .map(|body| body.error.code)
            .unwrap_or_default();
        Err(from_remote(response.status, &code))
    }

    fn note_success(&self) {
        let now = self.host.now_ms().ok();
        let mut health = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *health = Health {
            consecutive_failures: 0,
            failing_since: None,
            last_success: now,
            last_error: None,
        };
    }

    fn note_failure(&self, code: &'static str) {
        let now = self.host.now_ms().ok();
        let mut health = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        if health.failing_since.is_none() {
            health.failing_since = now;
        }
        health.last_error = Some(code);
    }
}

#[derive(Default)]
struct Taken {
    receipts: Vec<spot_link_exchange::Receipt>,
    refused: usize,
    failed: bool,
}

fn is_unattributable(error: &spot_link_exchange::Error) -> bool {
    use spot_link_exchange::codes as c;
    matches!(
        error.code(),
        c::IDENTITY_MISMATCH | c::LINK_MISMATCH | c::UNKNOWN_KEY | c::BAD_SIGNATURE
    )
}

fn receipt_error(error: &spot_link_exchange::Error) -> Error {
    if is_unattributable(error) {
        Error::peer_misbehaved()
    } else {
        Error::unavailable()
    }
}

/// A duration in the largest whole unit, for a sentence.
fn plain(ms: u64) -> String {
    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    let (n, unit) = if ms >= DAY {
        (ms / DAY, "day")
    } else if ms >= HOUR {
        (ms / HOUR, "hour")
    } else {
        ((ms / MINUTE).max(1), "minute")
    };
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}
