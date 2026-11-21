//! `migration.progress` over `spot-realtime`: the tenant room, the admin-only authorizer, and
//! the notification payload.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use spot_core::{Clock, SystemClock};
use spot_migration_apply::{ProgressEvent, ProgressSink, Version};
use spot_migration_apply_pg::{AdminOnly, MIGRATION_ROOM, PROGRESS_KIND, RealtimeProgress};
use spot_realtime::{BusConfig, Principal, RealtimeBus, RoomAuthorizer};
use support::*;

fn make_bus(authorizer: Arc<dyn RoomAuthorizer>) -> RealtimeBus {
    RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()) as Arc<dyn Clock>,
        authorizer,
    )
}

#[tokio::test]
async fn progress_is_published_to_the_tenants_migration_room_and_admins_receive_it() {
    let admin = Arc::new(AdminOnly::new(["alice"]));
    let bus = make_bus(admin);
    let sink = RealtimeProgress::new(bus.clone(), site()).unwrap();
    assert_eq!(sink.room().as_str(), format!("doctype:{MIGRATION_ROOM}"));

    let mut subscription = bus.subscribe(site(), Principal::user("alice"));
    subscription.join(sink.room(), None).await.unwrap();

    sink.on(ProgressEvent::Started {
        plan_digest: "plan-1".to_owned(),
        at: 1_000,
    });
    sink.on(ProgressEvent::StepStarted {
        index: 0,
        id: "schema/1".to_owned(),
        at: 1_100,
    });
    sink.on(ProgressEvent::Opened {
        version: Version::New,
        verified: true,
        at: 1_200,
    });

    let mut kinds = Vec::new();
    for _ in 0..3 {
        let event = tokio::time::timeout(Duration::from_secs(2), subscription.recv())
            .await
            .expect("an event arrives")
            .expect("the subscription is open");
        assert_eq!(event.room.as_str(), format!("doctype:{MIGRATION_ROOM}"));
        assert_eq!(event.kind, PROGRESS_KIND);
        kinds.push(event.payload["event"].as_str().unwrap().to_owned());
    }
    assert_eq!(kinds, ["started", "step_started", "opened"]);
}

#[tokio::test]
async fn a_non_admin_and_a_guest_are_denied_the_migration_room() {
    let admin = Arc::new(AdminOnly::new(["alice"]));
    let bus = make_bus(admin.clone());
    let sink = RealtimeProgress::new(bus.clone(), site()).unwrap();

    let mut bob = bus.subscribe(site(), Principal::user("bob"));
    let denied = bob
        .join(sink.room(), None)
        .await
        .expect_err("bob is not an admin");
    assert!(denied.to_string().contains("administrators"), "{denied}");

    let mut guest = bus.subscribe(site(), Principal::guest());
    let denied = guest
        .join(sink.room(), None)
        .await
        .expect_err("a guest is refused");
    assert!(denied.to_string().contains("guest"), "{denied}");

    // A permission revoked between connections is checked again on the next join: the same
    // principal is refused after the admin list changes on a new authorizer.
    let narrowed = Arc::new(AdminOnly::new(["carol"]));
    let bus2 = make_bus(narrowed);
    let sink2 = RealtimeProgress::new(bus2.clone(), site()).unwrap();
    let mut alice = bus2.subscribe(site(), Principal::user("alice"));
    assert!(alice.join(sink2.room(), None).await.is_err());
}

#[tokio::test]
async fn a_notification_carries_no_document_data_and_a_site_room_join_is_delegated() {
    let admin = Arc::new(AdminOnly::new(["alice"]));
    assert!(AdminOnly::is_migration_room(
        &spot_realtime::Room::doctype(MIGRATION_ROOM).unwrap()
    ));
    assert!(!AdminOnly::is_migration_room(&spot_realtime::Room::site()));

    // Another room is delegated to the inner authorizer (allow-all by default).
    let decision = admin
        .authorize(
            &site(),
            &Principal::user("bob"),
            &spot_realtime::Room::site(),
        )
        .await;
    assert!(decision.is_ok());

    let bus = make_bus(admin);
    let sink = RealtimeProgress::new(bus.clone(), site()).unwrap();
    let mut subscription = bus.subscribe(site(), Principal::user("alice"));
    subscription.join(sink.room(), None).await.unwrap();
    sink.on(ProgressEvent::GateClosed { at: 1_000 });
    let event = subscription.recv().await.unwrap();
    let text = serde_json::to_string(event.payload.as_ref()).unwrap();
    // The payload is the tag and the instant, never a document, a row or a secret.
    assert!(text.contains("gate_closed"), "{text}");
    assert!(!text.contains("password"), "{text}");
    assert!(!text.contains("select"), "{text}");
}

#[tokio::test]
async fn two_sites_have_independent_migration_rooms() {
    let bus = make_bus(Arc::new(AdminOnly::new(["alice"])));
    let one = RealtimeProgress::new(bus.clone(), site()).unwrap();
    let other_site = spot_core::SiteId::new("other-site").unwrap();
    let two = RealtimeProgress::new(bus.clone(), other_site.clone()).unwrap();
    assert_eq!(one.room(), two.room());

    let mut sub = bus.subscribe(site(), Principal::user("alice"));
    sub.join(one.room(), None).await.unwrap();
    two.on(ProgressEvent::GateClosed { at: 1_000 });
    // The event went to the other site's room, so this subscription has nothing.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sub.recv())
            .await
            .is_err()
    );
    one.on(ProgressEvent::GateClosed { at: 1_000 });
    assert!(
        tokio::time::timeout(Duration::from_secs(2), sub.recv())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_closed_subscription_never_fails_the_engine() {
    // A progress sink is best effort: publishing to a bus with no subscribers must not panic
    // or block, and the engine's journal stays the authority.
    let bus = make_bus(Arc::new(AdminOnly::new(["alice"])));
    let sink = RealtimeProgress::new(bus, site()).unwrap();
    for event in [
        ProgressEvent::Started {
            plan_digest: "p".to_owned(),
            at: 1,
        },
        ProgressEvent::Halted {
            reason: "x".to_owned(),
            at: 2,
        },
    ] {
        sink.on(event);
    }
}
