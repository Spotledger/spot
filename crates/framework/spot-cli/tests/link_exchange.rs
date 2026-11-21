//! The link exchange runner (WP-CTL-10.T9): two tenants over the in-process loopback carrier, the
//! real client and server of `spot-link-transport`, the real exchange stores in memory.
//!
//! The two-tenant fixture is the one of spot-link-transport's own tests, included by path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

#[path = "../../spot-link-transport/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use spot_cli::services::SiteCtx;
use spot_cli::services::link_exchange::{
    ApplyRefusal, ExchangeService, LinkExchangePorts, LinkPrincipal, LinkRecordApplier,
    PeerDirectory, ResilienceFactory, SendHold, Settings, apply_pending, backoff,
    dead_letter_sweep, register,
};
use spot_core::SystemClock;
use spot_link_exchange::{EventKind, InboxEntry, MessageParts, MessageState, OutboxStore};
use spot_link_exchange::{InboxStore as _, NoticeFeed as _};
use spot_link_transport::wire::CONTENT_TYPE;
use spot_link_transport::{
    CarrierTable, LinkCarriers, PeerTransport, PinnedCertificates, Resilience,
};
use spot_sync_http::{HttpRequest, LoopbackTransport, NoJitter, NoopSleeper, Transport};
use spot_tenant_link::{Carrier, Link, LinkState, Party};
use tokio::sync::{Semaphore, watch};

struct Peers {
    transport: Arc<dyn Transport>,
    peer: Party,
}

impl PeerDirectory for Peers {
    fn transport(&self, _link: &Link) -> Option<(PeerTransport, LinkCarriers)> {
        Some((
            PeerTransport::new(Arc::clone(&self.transport), self.peer.clone()),
            LinkCarriers::both(Carrier::Direct),
        ))
    }
}

struct Kit;

impl ResilienceFactory for Kit {
    fn make(&self, bulkhead: Arc<Semaphore>) -> Resilience {
        Resilience {
            jitter: Box::new(NoJitter),
            sleeper: Arc::new(NoopSleeper),
            bulkhead,
        }
    }
}

struct Hold(AtomicBool);

impl SendHold for Hold {
    fn held(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct Recorder {
    calls: AtomicUsize,
    verdict: Result<(), ApplyRefusal>,
}

impl LinkRecordApplier for Recorder {
    fn apply(
        &self,
        _principal: &LinkPrincipal,
        _link: &Link,
        _entry: &InboxEntry,
    ) -> Result<(), ApplyRefusal> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.verdict
    }
}

struct Rig {
    world: World,
    hold: Arc<Hold>,
    applier: Arc<Recorder>,
    service: Arc<ExchangeService>,
    transport: Arc<LoopbackTransport>,
    stop: watch::Receiver<bool>,
    _keep: watch::Sender<bool>,
}

/// Node A runs the service; node B serves the endpoints.
fn rig(settings: Settings) -> Rig {
    let world = world();
    let cert = certificate(1);
    let carriers = Arc::new(CarrierTable::new());
    carriers.set(world.a.link, LinkCarriers::both(Carrier::Direct));
    let (server_b, _) = server_for(&world.b, &world.a.party, &cert, carriers);
    let transport = Arc::new(LoopbackTransport::new(server_b.router(), cert));
    let hold = Arc::new(Hold(AtomicBool::new(false)));
    let applier = Arc::new(Recorder {
        calls: AtomicUsize::new(0),
        verdict: Ok(()),
    });
    let ports = Arc::new(LinkExchangePorts {
        host: world.a.host.clone(),
        local: world.a.party.clone(),
        certificates: Arc::new(PinnedCertificates::new()),
        carriers: Arc::new(CarrierTable::new()),
        peers: Arc::new(Peers {
            transport: transport.clone(),
            peer: world.b.party.clone(),
        }),
        resilience: Arc::new(Kit),
        hold: hold.clone(),
        applier: applier.clone(),
    });
    let (keep, stop) = watch::channel(false);
    Rig {
        world,
        hold,
        applier,
        service: Arc::new(ExchangeService::new(ports, settings)),
        transport,
        stop,
        _keep: keep,
    }
}

fn enabled() -> Settings {
    Settings {
        enabled: true,
        tick_ms: 50,
        ..Settings::default()
    }
}

async fn until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..250 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done()
}

#[test]
fn settings_are_off_by_default_and_validated() {
    let s = Settings::default();
    assert!(!s.enabled);
    s.validate().unwrap();
    for bad in [
        Settings {
            tick_ms: 0,
            ..Settings::default()
        },
        Settings {
            dead_letter_attempts: 0,
            ..Settings::default()
        },
        Settings {
            per_link_per_minute: 0,
            ..Settings::default()
        },
        Settings {
            apply_batch: 0,
            ..Settings::default()
        },
        Settings {
            max_in_flight: 0,
            ..Settings::default()
        },
    ] {
        assert!(bad.validate().is_err());
    }
    assert!(toml::from_str::<Settings>("enabled = true\nunknown = 1").is_err());
}

fn ctx(table: Option<&str>) -> SiteCtx<'static> {
    let mut settings = BTreeMap::new();
    if let Some(text) = table {
        settings.insert(
            "link_exchange".to_owned(),
            text.parse::<toml::Value>().unwrap(),
        );
    }
    SiteCtx::new(
        "site",
        Duration::from_secs(1),
        Arc::new(SystemClock::new()),
        settings,
        true,
    )
}

#[test]
fn disabled_registers_nothing_and_enabled_without_ports_refuses_to_start() {
    register(&mut ctx(None)).unwrap();
    register(&mut ctx(Some("enabled = false"))).unwrap();
    let error = register(&mut ctx(Some("enabled = true"))).unwrap_err();
    assert!(error.to_string().contains("LinkExchangePorts"), "{error}");
}

#[test]
fn backoff_doubles_and_is_capped() {
    let base = Duration::from_millis(200);
    let cap = Duration::from_secs(300);
    assert_eq!(backoff(0, base, cap), base);
    assert_eq!(backoff(1, base, cap), Duration::from_millis(400));
    assert_eq!(backoff(3, base, cap), Duration::from_millis(1_600));
    assert_eq!(backoff(40, base, cap), cap);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_is_delivered_once_with_only_granted_fields() {
    let r = rig(enabled());
    // `message.secret` is not in the grant: it is dropped at enqueue, before signing.
    let mut payload = body("hello");
    payload.insert(
        spot_tenant_link::FieldName::new("message.secret").unwrap(),
        spot_tenant_link::FieldValue::Text("do-not-send".to_owned()),
    );
    let link = r.world.a.current_link();
    let conversation = conversation("orders");
    let topic = message_topic();
    let now = r.world.a.host.now_ms().unwrap();
    let id = r
        .world
        .a
        .host
        .outbox()
        .enqueue_message(
            &spot_link_exchange::Submit {
                link: &link,
                conversation: &conversation,
                topic: &topic,
                schema_version: 1,
                now,
                actor: "user:alice",
                oracle: None,
            },
            &payload,
        )
        .unwrap();
    assert_eq!(id.withheld, vec!["message.secret".to_owned()]);

    r.service.tick(&r.stop).unwrap();
    let a = &r.world.a;
    assert!(until(|| a.states() == vec![MessageState::Applied]).await);

    let entry = r
        .world
        .b
        .inbox
        .entry(&r.world.b.link, &id.id)
        .unwrap()
        .unwrap();
    assert!(!entry.message.payload.contains("secret"));
    assert!(!entry.message.payload.contains("do-not-send"));
    // Another pass delivers nothing more: one notice, one entry.
    r.service.tick(&r.stop).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(r.world.b.inbox.read_from(0, 10).unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn revocation_stops_delivery() {
    let r = rig(enabled());
    r.world.a.enqueue("orders", "hello");
    r.world.a.revoke();
    r.service.tick(&r.stop).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.service.running(), 0);
    assert_eq!(r.world.a.states(), vec![MessageState::Queued]);
    assert_ne!(
        r.world
            .a
            .current_link()
            .state_at(r.world.a.host.now_ms().unwrap()),
        LinkState::Active
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_task_ends_when_the_link_is_revoked() {
    let r = rig(enabled());
    r.service.tick(&r.stop).unwrap();
    assert!(r.service.running() >= 1);
    r.world.a.revoke();
    // The supervisor's next pass aborts the tasks of a link that is no longer active.
    r.service.tick(&r.stop).unwrap();
    assert!(until(|| r.service.running() == 0).await);
    // Nothing leaves afterwards either: the link refuses enqueue, so the outbox stays empty.
    assert!(r.world.a.states().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_effect_hold_suspends_delivery_until_released() {
    let r = rig(enabled());
    r.hold.0.store(true, Ordering::SeqCst);
    r.world.a.enqueue("orders", "hello");
    r.service.tick(&r.stop).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.service.running(), 0);
    assert_eq!(r.world.a.states(), vec![MessageState::Queued]);
    r.hold.0.store(false, Ordering::SeqCst);
    r.service.tick(&r.stop).unwrap();
    let a = &r.world.a;
    assert!(until(|| a.states() == vec![MessageState::Applied]).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_handed_over_too_often_is_dead_lettered() {
    let w = world();
    let young = w.a.enqueue("orders", "young");
    let old = w.a.enqueue("billing", "old");
    let mut entry = w.a.outbox.find(&old).unwrap().unwrap();
    entry.state = MessageState::Sent;
    entry.attempts = 5;
    w.a.outbox.update(entry).unwrap();
    let now = w.a.host.now_ms().unwrap();

    assert!(dead_letter_sweep(&w.a.host, 6, now).unwrap().is_empty());
    let done = dead_letter_sweep(&w.a.host, 5, now).unwrap();
    assert_eq!(done, vec![old]);
    let settled = w.a.outbox.find(&old).unwrap().unwrap();
    assert_eq!(settled.state, MessageState::Expired);
    assert!(
        settled.filler.is_some(),
        "the slot is filled by a tombstone"
    );
    assert_eq!(
        w.a.outbox.find(&young).unwrap().unwrap().state,
        MessageState::Queued
    );
    let events = w.a.outbox.events_from(0, 10).unwrap();
    assert!(
        events
            .iter()
            .any(|(_, e)| e.message == old && e.kind == EventKind::Expired)
    );
    // A second pass finds nothing new.
    assert!(dead_letter_sweep(&w.a.host, 5, now).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inbound_field_outside_the_grant_is_not_applied() {
    let r = rig(enabled());
    let a = &r.world.a;
    let link = a.current_link();
    let now = a.host.now_ms().unwrap();
    let id = a.host.ids.next_id(now);
    // A message A signed with a field the grant does not list: it cannot come from A's outbox
    // (which filters at enqueue), so it is built by hand.
    let message = spot_link_exchange::Message::sign(
        MessageParts {
            id,
            link: link.id,
            conversation: conversation("orders"),
            seq: 1,
            topic: message_topic(),
            schema_version: 1,
            sent_at: now,
            expires_at: now + 86_400_000,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":"x","message.secret":"y"}"#.to_owned(),
        },
        a.signer.as_ref(),
    )
    .unwrap();
    let request = HttpRequest::post(
        format!("/link/v1/{}/messages", link.id),
        serde_json::to_vec(&message).unwrap(),
    )
    .with_content_type(CONTENT_TYPE);
    let _ = r.transport.request(request).await;
    let entry = r.world.b.inbox.entry(&r.world.b.link, &id).unwrap();
    assert!(
        entry.is_none_or(|e| e.status != spot_link_exchange::EntryStatus::Applied),
        "a field outside the grant must never be applied"
    );
}

#[test]
fn the_link_principal_carries_the_peer_and_exactly_the_granted_topics() {
    let w = world();
    let link = w.a.current_link();
    let principal = LinkPrincipal::of(&link).unwrap();
    assert_eq!(principal.party, w.b.party);
    assert_eq!(principal.topics, vec!["bpm.message".to_owned()]);
    assert!(principal.may_write("bpm.message"));
    assert!(!principal.may_write("hr.payroll"));
}

#[tokio::test(flavor = "multi_thread")]
async fn apply_pending_with_nothing_pending_does_nothing_and_never_calls_the_engine() {
    let r = rig(enabled());
    let report = apply_pending(&r.world.a.host, r.applier.as_ref(), 10, 1).unwrap();
    assert_eq!((report.applied, report.refused, report.deferred), (0, 0, 0));
    assert_eq!(r.applier.calls.load(Ordering::SeqCst), 0);
}
