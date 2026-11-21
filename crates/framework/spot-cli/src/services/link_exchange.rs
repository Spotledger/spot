//! The link exchange runner (WP-CTL-10.T9, ADR-058 rules 4, 8, 9, 11, 13; ADR-080): the optional
//! tenant-host service that moves the signed messages of `spot-link-exchange` over the direct and
//! pull carriers of `spot-link-transport`, off by default (`[services.link_exchange] enabled`).
//!
//! The libraries already own the delivery logic: the outbox signs once, applies the grant's field
//! list **at enqueue, before serialisation** (a field not granted is never in the signed payload,
//! so it cannot leave the process), numbers, resends and expires; the inbox authenticates,
//! deduplicates, orders and checks the receiver's grant; the client carries the breaker, retries
//! and the resend cadence. This module runs them:
//!
//! - **Outbound.** One task per active link and direction. Direct links call
//!   `LinkClient::push_step`, pull links `pull_step`. Each step is charged to a per-link token
//!   bucket; a failed step backs off exponentially up to the client's retry cap. Message ids are
//!   stable (one signed message, resent byte for byte) and the receiver deduplicates on the id.
//! - **Revocation and hold.** Every iteration re-reads the link: a link that is not `Active` ends
//!   its tasks at once (and the supervisor aborts them on its next tick). While the effect hold is
//!   on (or its state is unknown: fail closed) nothing leaves; queued messages wait.
//! - **Dead letters.** A message handed over [`Settings::dead_letter_attempts`] times without
//!   confirmation is expired early with its tombstone and a producer event, exactly as lifetime
//!   expiry does, so later messages in its conversation are not held behind its slot.
//! - **Inbound.** The mounted [`LinkServer`] authenticates the peer from the transport and the
//!   inbox rejects any field outside the grant. A record then waits as pending; [`apply_pending`]
//!   hands it to the host's [`LinkRecordApplier`] as the **link principal** ([`LinkPrincipal`]):
//!   the peer party with exactly the grant's topics, never a system actor. A link revoked meanwhile
//!   never produces an applied row.
//!
//! Nothing here reads the wall clock (the host's clock supplies every instant), draws randomness,
//! uses floats, or logs a payload, key or certificate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use spot_link_exchange::{
    EventKind, Filler, InboxEntry, MessageId, MessageParts, MessageState, ProducerEvent,
    TOMBSTONE_EXPIRES, TOMBSTONE_PAYLOAD,
};
use spot_link_transport::{
    CarrierTable, ClientConfig, LinkCarriers, LinkClient, LinkHost, LinkServer, PeerCertificates,
    PeerTransport, RateConfig, RateLimiter, Resilience, ServerSettings,
};
use spot_tenant_link::{Carrier, Link, LinkId, LinkState, Party};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the links service (1400), which owns the link store.
pub const ORDER: u32 = 1500;

/// `[services.link_exchange]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the runner runs. Off by default: no task, no route, no network call.
    pub enabled: bool,
    /// How often the supervisor looks at the links, the dead letters and the pending records.
    pub tick_ms: u64,
    /// Hand-overs without confirmation after which a message is dead-lettered. At least 1.
    pub dead_letter_attempts: u32,
    /// Steps per link and minute, each direction, outbound and served inbound alike. At least 1.
    pub per_link_per_minute: u32,
    /// Pending records applied per tick. At least 1.
    pub apply_batch: usize,
    /// Calls in flight across all links. At least 1.
    pub max_in_flight: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            tick_ms: 1_000,
            dead_letter_attempts: 20,
            per_link_per_minute: 600,
            apply_batch: 32,
            max_in_flight: 16,
        }
    }
}

impl Settings {
    /// Checks the ranges.
    ///
    /// # Errors
    /// A message naming the setting, never a value.
    pub fn validate(&self) -> Result<()> {
        let bad = |name: &str| Error(format!("services.link_exchange.{name} must be at least 1"));
        if self.tick_ms == 0 {
            return Err(bad("tick_ms"));
        }
        if self.dead_letter_attempts == 0 {
            return Err(bad("dead_letter_attempts"));
        }
        if self.per_link_per_minute == 0 {
            return Err(bad("per_link_per_minute"));
        }
        if self.apply_batch == 0 {
            return Err(bad("apply_batch"));
        }
        if self.max_in_flight == 0 {
            return Err(bad("max_in_flight"));
        }
        Ok(())
    }

    fn rate(&self) -> RateConfig {
        RateConfig {
            burst: self.per_link_per_minute,
            per_second: (self.per_link_per_minute / 60).max(1),
            max_keys: 1_024,
        }
    }
}

/// Where a link's peer is reached and which carrier each direction uses. The directory of peers
/// (pinned certificates, endpoints) belongs to the module that owns the link configuration.
pub trait PeerDirectory: Send + Sync {
    /// The transport pinned to the peer of `link` and the carriers of its two directions, or `None`
    /// when this host has no endpoint for that peer (the link is then left alone).
    fn transport(&self, link: &Link) -> Option<(PeerTransport, LinkCarriers)>;
}

/// Builds the injected parts of the resilience kit (jitter, sleeper) for one link's client; the
/// bulkhead is shared by every link and passed in.
pub trait ResilienceFactory: Send + Sync {
    /// A fresh kit sharing `bulkhead`.
    fn make(&self, bulkhead: Arc<Semaphore>) -> Resilience;
}

/// The side-effect hold (WP-MIG-08.T1) as the runner sees it. An adapter over the hold gate
/// answers; an unreadable or unknown state must answer `true` (fail closed).
pub trait SendHold: Send + Sync {
    /// Whether external calls are held right now.
    fn held(&self) -> bool;
}

/// Who an inbound record is applied as: the peer of the link, with exactly the topics its grant
/// names. It is **not** a system actor and carries no right outside the grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkPrincipal {
    /// The link the record arrived on.
    pub link: LinkId,
    /// The peer party the connection authenticated.
    pub party: Party,
    /// The topics the grant allows, nothing else.
    pub topics: Vec<String>,
}

impl LinkPrincipal {
    /// The principal of `link`, or `None` when the link has no peer yet.
    #[must_use]
    pub fn of(link: &Link) -> Option<Self> {
        let party = link.peer()?.clone();
        let topics = link
            .grants
            .iter()
            .map(|grant| grant.topic.as_str().to_owned())
            .collect();
        Some(Self {
            link: link.id,
            party,
            topics,
        })
    }

    /// Whether the principal may write `topic`.
    #[must_use]
    pub fn may_write(&self, topic: &str) -> bool {
        self.topics.iter().any(|t| t == topic)
    }
}

/// Why the engine did not apply a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyRefusal {
    /// The record is refused for good (shape, ownership, permission): the sender gets a rejection.
    Refused,
    /// Not now (storage busy): the record stays pending and is tried again.
    Transient,
}

/// The engine side of an inbound record: applies it through the replica apply path as
/// `principal`. The adapter decodes the entry with `Inbox::received_record` and must refuse any
/// write the principal's topics do not cover.
pub trait LinkRecordApplier: Send + Sync {
    /// Applies one pending record.
    ///
    /// # Errors
    /// [`ApplyRefusal`].
    fn apply(
        &self,
        principal: &LinkPrincipal,
        link: &Link,
        entry: &InboxEntry,
    ) -> std::result::Result<(), ApplyRefusal>;
}

/// What the runner needs from the rest of the host. Provide one with
/// `SiteCtx::provide::<LinkExchangePorts>` from the module that owns the stores and the peers;
/// with `[services.link_exchange]` enabled and no module providing it the site refuses to start.
pub struct LinkExchangePorts {
    /// The stores, signer, clock and tuning of this side.
    pub host: LinkHost,
    /// This side's party (the links are listed by it).
    pub local: Party,
    /// Maps a presented client certificate to its pinned party (inbound).
    pub certificates: Arc<dyn PeerCertificates>,
    /// Per-link carrier overrides (inbound).
    pub carriers: Arc<CarrierTable>,
    /// The peers to dial (outbound).
    pub peers: Arc<dyn PeerDirectory>,
    /// The resilience kit factory.
    pub resilience: Arc<dyn ResilienceFactory>,
    /// The effect hold.
    pub hold: Arc<dyn SendHold>,
    /// The engine's apply path.
    pub applier: Arc<dyn LinkRecordApplier>,
}

/// What one [`apply_pending`] pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Records applied and marked.
    pub applied: usize,
    /// Records refused (receiver rejection sent back through the receipt path).
    pub refused: usize,
    /// Records left pending: link not active, no peer, engine busy.
    pub deferred: usize,
}

/// Applies the records waiting in the inbox, as their link's principal.
///
/// # Errors
/// A message when the inbox cannot be read.
pub fn apply_pending(
    host: &LinkHost,
    applier: &dyn LinkRecordApplier,
    limit: usize,
    now: u64,
) -> std::result::Result<ApplyReport, String> {
    let inbox = host.inbox();
    let pending = inbox
        .pending_records(limit)
        .map_err(|_| "the inbox could not be read".to_owned())?;
    let mut report = ApplyReport::default();
    for item in pending {
        let id: MessageId = item.entry.message.id;
        let Ok(link) = host.links.get(&item.link) else {
            report.deferred += 1;
            continue;
        };
        if link.state_at(now) != LinkState::Active {
            report.deferred += 1;
            continue;
        }
        let Some(principal) = LinkPrincipal::of(&link) else {
            report.deferred += 1;
            continue;
        };
        let topic = item.entry.message.topic.as_str().to_owned();
        let verdict = if principal.may_write(&topic) {
            applier.apply(&principal, &link, &item.entry)
        } else {
            Err(ApplyRefusal::Refused)
        };
        match verdict {
            Ok(()) => match inbox.record_applied(&link, &id, now) {
                Ok(_) => report.applied += 1,
                Err(_) => report.deferred += 1,
            },
            Err(ApplyRefusal::Refused) => match inbox.record_refused(&link, &id, now) {
                Ok(_) => report.refused += 1,
                Err(_) => report.deferred += 1,
            },
            Err(ApplyRefusal::Transient) => report.deferred += 1,
        }
    }
    Ok(report)
}

/// Expires, with their tombstones and producer events, the messages handed over `limit` times
/// without a confirmation. Returns the ids dead-lettered. A conflicting concurrent update is
/// skipped and retried on the next pass.
///
/// # Errors
/// A message when the outbox cannot be read.
pub fn dead_letter_sweep(
    host: &LinkHost,
    limit: u32,
    now: u64,
) -> std::result::Result<Vec<MessageId>, String> {
    let live = host
        .outbox
        .live(None)
        .map_err(|_| "the outbox could not be read".to_owned())?;
    let mut done = Vec::new();
    for mut entry in live {
        let open = matches!(entry.state, MessageState::Queued | MessageState::Sent);
        if !open || entry.attempts < limit {
            continue;
        }
        let original = entry.message.clone();
        let tombstone = spot_link_exchange::Message::sign(
            MessageParts {
                id: host.ids.next_id(now),
                link: original.link,
                conversation: original.conversation.clone(),
                seq: original.seq,
                topic: original.topic.clone(),
                schema_version: original.schema_version,
                sent_at: now.max(original.sent_at.saturating_add(1)),
                expires_at: TOMBSTONE_EXPIRES,
                tombstone: true,
                replaces: Some(original.id),
                payload: TOMBSTONE_PAYLOAD.to_owned(),
            },
            host.signer.as_ref(),
        )
        .map_err(|_| "a tombstone could not be signed".to_owned())?;
        entry.state = MessageState::Expired;
        entry.state_at = now;
        entry.filler = Some(Filler {
            message: tombstone,
            acknowledged: false,
            last_sent_at: None,
        });
        let event = ProducerEvent {
            message: original.id,
            link: original.link,
            conversation: original.conversation.clone(),
            seq: original.seq,
            topic: original.topic.clone(),
            kind: EventKind::Expired,
            at: now,
        };
        if host.outbox.settle(entry, Some(event)).is_ok() {
            done.push(original.id);
        }
    }
    Ok(done)
}

/// The pause after `failures` failed steps in a row: doubling from `base`, capped.
#[must_use]
pub fn backoff(failures: u32, base: Duration, cap: Duration) -> Duration {
    let shift = failures.min(20);
    let factor = 1u32 << shift;
    base.saturating_mul(factor).min(cap)
}

/// The runner: a supervisor that starts and stops the per-link tasks.
pub struct ExchangeService {
    ports: Arc<LinkExchangePorts>,
    settings: Settings,
    bulkhead: Arc<Semaphore>,
    rate: Arc<RateLimiter>,
    tasks: Mutex<HashMap<(LinkId, bool), JoinHandle<()>>>,
}

impl ExchangeService {
    /// A runner over `ports`.
    #[must_use]
    pub fn new(ports: Arc<LinkExchangePorts>, settings: Settings) -> Self {
        let rate = Arc::new(RateLimiter::new(settings.rate()));
        let bulkhead = Arc::new(Semaphore::new(settings.max_in_flight));
        Self {
            ports,
            settings,
            bulkhead,
            rate,
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// The number of per-link tasks running (for readiness and tests).
    #[must_use]
    pub fn running(&self) -> usize {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|handle| !handle.is_finished())
            .count()
    }

    /// One supervisor pass: dead letters, pending records, and the per-link tasks.
    ///
    /// # Errors
    /// A message when the clock or a store cannot be read.
    pub fn tick(&self, stop: &watch::Receiver<bool>) -> std::result::Result<(), String> {
        let host = &self.ports.host;
        let now = host
            .now_ms()
            .map_err(|_| "the clock cannot be trusted".to_owned())?;
        dead_letter_sweep(host, self.settings.dead_letter_attempts, now)?;
        apply_pending(
            host,
            self.ports.applier.as_ref(),
            self.settings.apply_batch,
            now,
        )?;
        let links = host
            .links
            .list_for(&self.ports.local)
            .map_err(|_| "the links could not be listed".to_owned())?;
        let held = self.ports.hold.held();
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.retain(|_, handle| !handle.is_finished());
        let mut wanted: Vec<(LinkId, bool)> = Vec::new();
        for link in &links {
            if held || link.state_at(now) != LinkState::Active {
                continue;
            }
            let Some((peer, carriers)) = self.ports.peers.transport(link) else {
                continue;
            };
            for push in [true, false] {
                let carrier = if push {
                    carriers.send
                } else {
                    carriers.receive
                };
                let runs_here = if push {
                    carrier == Carrier::Direct
                } else {
                    carrier == Carrier::Pull
                };
                if !runs_here {
                    continue;
                }
                wanted.push((link.id, push));
                if tasks.contains_key(&(link.id, push)) {
                    continue;
                }
                let resilience = self.ports.resilience.make(Arc::clone(&self.bulkhead));
                let Ok(client) = LinkClient::new(
                    host.clone(),
                    link.id,
                    peer.clone(),
                    carriers,
                    ClientConfig::default(),
                    resilience,
                ) else {
                    continue;
                };
                let handle = tokio::spawn(pump(
                    Arc::new(client),
                    link.id,
                    push,
                    Arc::clone(&self.ports),
                    Arc::clone(&self.rate),
                    stop.clone(),
                ));
                tasks.insert((link.id, push), handle);
            }
        }
        // Revoked, expired, held or no longer configured: stop now.
        tasks.retain(|key, handle| {
            if wanted.contains(key) {
                true
            } else {
                handle.abort();
                false
            }
        });
        Ok(())
    }

    fn abort_all(&self) {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, handle) in tasks.drain() {
            handle.abort();
        }
    }
}

/// One direction of one link, until it is stopped, the link ends or the hold is on.
async fn pump(
    client: Arc<LinkClient>,
    link: LinkId,
    push: bool,
    ports: Arc<LinkExchangePorts>,
    rate: Arc<RateLimiter>,
    stop: watch::Receiver<bool>,
) {
    let config = ClientConfig::default();
    let key = format!("{link}:{}", if push { "push" } else { "pull" });
    let mut failures: u32 = 0;
    loop {
        if *stop.borrow() {
            return;
        }
        // Revocation or expiry ends the task at once; the supervisor would also abort it.
        let active = ports
            .host
            .now_ms()
            .ok()
            .zip(ports.host.links.get(&link).ok())
            .is_some_and(|(now, current)| current.state_at(now) == LinkState::Active);
        if !active {
            return;
        }
        if ports.hold.held() {
            tokio::time::sleep(config.idle_wait).await;
            continue;
        }
        if let Err(wait) = rate.take(&key, ports.host.clock.monotonic()) {
            tokio::time::sleep(wait.max(Duration::from_millis(10))).await;
            continue;
        }
        let step = if push {
            client.push_step().await
        } else {
            client.pull_step().await
        };
        let pause = match step {
            Ok(report) => {
                failures = 0;
                if report.moved > 0 || !push {
                    Duration::ZERO
                } else {
                    config.idle_wait
                }
            }
            Err(_) => {
                failures = failures.saturating_add(1);
                backoff(failures, config.retry.base, config.retry.cap)
            }
        };
        if pause.is_zero() {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(pause).await;
        }
    }
}

impl Service for ExchangeService {
    fn name(&self) -> &'static str {
        "link_exchange"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.ports
                .host
                .links
                .list_for(&self.ports.local)
                .map(|_| ())
                .map_err(|_| "the link store could not be read".to_owned())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let tick = Duration::from_millis(self.settings.tick_ms);
            loop {
                if let Err(reason) = self.tick(&stop) {
                    tracing::warn!(reason = %reason, "link exchange pass failed");
                }
                tokio::select! {
                    () = tokio::time::sleep(tick) => {}
                    _ = stop.changed() => {
                        self.abort_all();
                        return Ok(());
                    }
                }
                if *stop.borrow() {
                    self.abort_all();
                    return Ok(());
                }
            }
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            self.abort_all();
            Ok(())
        })
    }
}

/// Registers the runner when `[services.link_exchange]` is enabled: mounts the link endpoints
/// (`/link/v1/...`) and adds the supervisor.
///
/// # Errors
/// A setting out of range, or a missing [`LinkExchangePorts`].
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("link_exchange")?;
    if !settings.enabled {
        return Ok(());
    }
    settings.validate()?;
    let ports = ctx.require_port::<LinkExchangePorts>()?;
    let server = LinkServer::new(
        ports.host.clone(),
        Arc::clone(&ports.certificates),
        Arc::clone(&ports.carriers),
    )
    .with_settings(ServerSettings {
        link_rate: settings.rate(),
        ..ServerSettings::default()
    });
    let router = server.router();
    ctx.router_layer(move |base| base.merge(router));
    ctx.add(Arc::new(ExchangeService::new(ports, settings)));
    Ok(())
}
