//! The authorisation seam: denials, per-join calls, and no lock across the await.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spot_core::{SiteId, SystemClock};
use spot_realtime::{
    AllowAll, AuthFuture, BusConfig, Closed, Denied, JoinError, Principal, RealtimeBus, Room,
};

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|e| panic!("test site name should be valid: {e}"))
}

fn room(name: &str) -> Room {
    Room::doctype(name).unwrap_or_else(|e| panic!("valid room: {e}"))
}

/// Denies every room listed in `denied`, counts every `authorize` call.
struct Recorder {
    denied: Vec<String>,
    calls: AtomicUsize,
}

impl Recorder {
    fn new(denied: &[&str]) -> Self {
        Self {
            denied: denied.iter().map(|d| (*d).to_string()).collect(),
            calls: AtomicUsize::new(0),
        }
    }

    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl spot_realtime::RoomAuthorizer for Recorder {
    fn authorize(&self, _site: &SiteId, _principal: &Principal, room: &Room) -> AuthFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let denied = self.denied.iter().any(|d| d == room.as_str());
        let outcome = if denied {
            Err(Denied::new("test policy denies this room"))
        } else {
            Ok(())
        };
        Box::pin(std::future::ready(outcome))
    }
}

#[tokio::test]
async fn denied_join_errors_and_the_subscriber_receives_nothing_from_the_room() {
    let recorder = Arc::new(Recorder::new(&["doctype:Secret"]));
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::clone(&recorder) as Arc<dyn spot_realtime::RoomAuthorizer>,
    );
    let site = site("acme.example");
    let open = room("Item");
    let secret = room("Secret");

    let mut sub = bus.subscribe(site.clone(), Principal::guest());
    let err = sub
        .join(&secret, None)
        .await
        .err_or_denied("denied join must fail");
    assert!(
        matches!(err, JoinError::Denied(_)),
        "expected JoinError::Denied, got {err:?}"
    );
    // The room the principal may see still works.
    sub.join(&open, None)
        .await
        .unwrap_or_else(|e| panic!("open join: {e}"));

    bus.publish(
        &site,
        &secret,
        "leak",
        serde_json::json!({ "hint": "no data" }),
    );
    let last = bus.publish(&site, &open, "doc_updated", serde_json::json!(1));

    let event = tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .map_or_else(
            |_| panic!("the open-room event must arrive"),
            |res| res.unwrap_or_else(|c| panic!("closed: {c}")),
        );
    assert_eq!(event.seq, last, "only the open room delivered");
    assert_eq!(event.room, open);
    let nothing = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .is_ok_and(|res| res.is_ok());
    assert!(!nothing, "denied room must deliver nothing");
}

#[tokio::test]
async fn the_authorizer_is_called_again_on_a_resumed_join() {
    let recorder = Arc::new(Recorder::new(&[]));
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::clone(&recorder) as Arc<dyn spot_realtime::RoomAuthorizer>,
    );
    let site = site("acme.example");
    let feed = room("Item");

    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    sub.join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("first join: {e}"));
    assert_eq!(recorder.count(), 1, "one join, one authorize");

    // Same subscription re-joining with a resume point still asks.
    sub.leave(&feed);
    sub.join(&feed, Some(0))
        .await
        .unwrap_or_else(|e| panic!("resumed join: {e}"));
    assert_eq!(recorder.count(), 2, "resume join must re-authorize");

    // A fresh subscription "after reconnect" asks again too.
    let mut sub2 = bus.subscribe(site, Principal::user("alice"));
    sub2.join(&feed, Some(0))
        .await
        .unwrap_or_else(|e| panic!("reconnect join: {e}"));
    assert_eq!(recorder.count(), 3, "every join is authorized");
}

/// An authorizer that parks — proving the bus does not hold a `std::sync` lock across the
/// `authorize` await: `publish` must sail through while a join is in flight.
struct Slow {
    delay: Duration,
}

impl spot_realtime::RoomAuthorizer for Slow {
    fn authorize(&self, _site: &SiteId, _principal: &Principal, _room: &Room) -> AuthFuture<'_> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(())
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_is_not_blocked_by_an_in_flight_authorizer() {
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(Slow {
            delay: Duration::from_millis(250),
        }),
    );
    let site = site("acme.example");
    let feed = room("Item");
    let mut joiner = bus.subscribe(site.clone(), Principal::user("alice"));
    let joined_room = feed.clone();
    let join = tokio::spawn(async move { joiner.join(&joined_room, None).await });

    // Let the authorizer park, then publish 100 events. If the bus held its state lock
    // across the authorize await, this loop would have to wait out the remaining sleep.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = std::time::Instant::now();
    for i in 1..=100u64 {
        assert_eq!(bus.publish(&site, &feed, "e", serde_json::json!(i)), i);
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(150),
        "publish waited {elapsed:?} for an in-flight authorizer — a lock is held across the await"
    );

    join.await.map_or_else(
        |_| panic!("join task failed"),
        |res| res.unwrap_or_else(|e| panic!("slow join should succeed: {e}")),
    );
}

#[tokio::test]
async fn join_on_a_closed_subscription_is_rejected() {
    let bus = RealtimeBus::new(
        BusConfig {
            queue_capacity: 1,
            ..BusConfig::default()
        },
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");
    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    sub.join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    // Overflow the single-slot queue from a second subscription's join? Simpler: publish
    // twice without reading — the subscriber becomes a slow consumer.
    bus.publish(&site, &feed, "a", serde_json::json!(null));
    bus.publish(&site, &feed, "b", serde_json::json!(null));
    let closed = tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .unwrap_or_else(|_| panic!("close must be reported"))
        .err_or_denied("expected Err(Closed)");
    assert_eq!(closed, Closed::SlowConsumer);
    let err = sub
        .join(&room("Other"), None)
        .await
        .err_or_denied("closed subscription cannot join");
    assert!(matches!(err, JoinError::Closed), "got {err:?}");
}

/// Unwraps the `Err` side of a result, panicking with `why` when it is `Ok`.
trait ErrOrDenied<T, E> {
    fn err_or_denied(self, why: &str) -> E;
}

impl<T, E> ErrOrDenied<T, E> for Result<T, E> {
    fn err_or_denied(self, why: &str) -> E {
        match self {
            Err(error) => error,
            Ok(_) => panic!("{why}: expected an error, got Ok"),
        }
    }
}
