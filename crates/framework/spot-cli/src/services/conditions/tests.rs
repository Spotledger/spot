#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use time::macros::datetime;

/// An in-memory store: records what it was asked to write, can be taken down, and refuses a
/// subject named `poison` for good (as a stored value out of range would).
#[derive(Default)]
struct Fake {
    down: AtomicBool,
    load_fails: AtomicBool,
    written: Mutex<Vec<String>>,
}

impl ConditionStore for Fake {
    fn load_open(&self) -> StoreFuture<'_, Vec<Open>> {
        Box::pin(async move {
            if self.load_fails.load(Ordering::SeqCst) {
                return Err(ConditionsPgError::Unavailable);
            }
            Ok(Vec::new())
        })
    }

    fn apply<'a>(&'a self, events: &'a [AuditEvent]) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(ConditionsPgError::Unavailable);
            }
            if events.iter().any(|e| e.subject().as_str() == "poison") {
                return Err(ConditionsPgError::Corrupt);
            }
            let mut written = self.written.lock().unwrap();
            written.extend(events.iter().map(|e| e.subject().as_str().to_owned()));
            Ok(())
        })
    }
}

fn settings() -> Settings {
    Settings {
        tick_seconds: 1,
        amber_needs_person_after_seconds: 300,
    }
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-01-01 00:00:00 UTC)))
}

fn now(clock: &ManualClock) -> Millis {
    let millis = clock.now_utc().unix_timestamp_nanos() / 1_000_000;
    Millis::new(u64::try_from(millis).unwrap())
}

async fn start(
    store: Arc<Fake>,
    clock: &Arc<ManualClock>,
) -> (
    Arc<ConditionsService>,
    ConditionsHandle,
    watch::Sender<bool>,
) {
    let (service, handle) = build(
        "acme".to_owned(),
        store,
        Arc::clone(clock) as Arc<dyn Clock>,
        settings(),
        Arc::new(ConditionRegistrations::default()),
    );
    service.check().await.unwrap();
    let (stop, rx) = watch::channel(false);
    let runner = Arc::clone(&service);
    tokio::spawn(async move {
        let _ = runner.run(rx).await;
    });
    (service, handle, stop)
}

async fn tick() {
    tokio::time::sleep(Duration::from_millis(1_500)).await;
}

async fn raise(handle: &ConditionsHandle, clock: &ManualClock, subject: &str) {
    handle
        .raise("NODE_SILENT", subject, ConditionParams::new(), now(clock))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_raise_does_not_wait_for_the_store_and_lands_on_the_tick() {
    let store = Arc::new(Fake::default());
    let clock = clock();
    let (_service, handle, stop) = start(Arc::clone(&store), &clock).await;
    raise(&handle, &clock, "branch-1").await;
    assert!(
        store.written.lock().unwrap().is_empty(),
        "a raise must not write inline"
    );
    tick().await;
    assert_eq!(*store.written.lock().unwrap(), ["branch-1"]);
    let _ = stop.send(true);
}

#[tokio::test]
async fn an_outage_keeps_the_order_and_lands_everything_once_the_store_returns() {
    let store = Arc::new(Fake::default());
    let clock = clock();
    let (_service, handle, stop) = start(Arc::clone(&store), &clock).await;
    store.down.store(true, Ordering::SeqCst);
    for subject in ["branch-1", "branch-2", "branch-3"] {
        raise(&handle, &clock, subject).await;
    }
    tick().await;
    assert!(handle.readiness_text().await.unwrap().starts_with("degraded:"));
    assert!(store.written.lock().unwrap().is_empty());
    store.down.store(false, Ordering::SeqCst);
    tick().await;
    assert_eq!(
        *store.written.lock().unwrap(),
        ["branch-1", "branch-2", "branch-3"]
    );
    let text = handle.readiness_text().await.unwrap();
    assert!(text.starts_with("conditions:"), "{text}");
    let _ = stop.send(true);
}

#[tokio::test]
async fn a_poison_event_is_set_aside_and_does_not_block_the_rest() {
    let store = Arc::new(Fake::default());
    let clock = clock();
    let (_service, handle, stop) = start(Arc::clone(&store), &clock).await;
    for subject in ["branch-1", "poison", "branch-2"] {
        raise(&handle, &clock, subject).await;
    }
    tick().await;
    assert_eq!(*store.written.lock().unwrap(), ["branch-1", "branch-2"]);
    let text = handle.readiness_text().await.unwrap();
    assert!(text.contains("rejected 1"), "{text}");
    let _ = stop.send(true);
}

#[tokio::test]
async fn a_stop_writes_what_is_still_queued() {
    let store = Arc::new(Fake::default());
    let clock = clock();
    let (_service, handle, stop) = start(Arc::clone(&store), &clock).await;
    raise(&handle, &clock, "branch-9").await;
    let _ = stop.send(true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*store.written.lock().unwrap(), ["branch-9"]);
    drop(handle);
}

#[tokio::test]
async fn a_service_that_failed_its_check_answers_errors_not_calm() {
    let store = Arc::new(Fake::default());
    store.load_fails.store(true, Ordering::SeqCst);
    let (service, handle) = build(
        "acme".to_owned(),
        store,
        clock() as Arc<dyn Clock>,
        settings(),
        Arc::new(ConditionRegistrations::default()),
    );
    assert_eq!(service.level(), Level::Optional);
    let reason = service.check().await.unwrap_err();
    assert!(reason.contains("spot migrate"), "{reason}");
    let view = SiteView {
        site: "acme".to_owned(),
        handle: handle.clone(),
        thresholds: settings().thresholds(),
    };
    assert!(view.health("acme").await.is_err(), "a dead service is not calm");
    assert!(handle.open().await.is_err());
}

#[tokio::test]
async fn the_view_refuses_another_tenant() {
    let store = Arc::new(Fake::default());
    let clock = clock();
    let (_service, handle, stop) = start(store, &clock).await;
    let view = SiteView {
        site: "acme".to_owned(),
        handle,
        thresholds: settings().thresholds(),
    };
    assert!(view.health("acme").await.unwrap().is_calm());
    assert!(view.health("other").await.is_err());
    let _ = stop.send(true);
}
