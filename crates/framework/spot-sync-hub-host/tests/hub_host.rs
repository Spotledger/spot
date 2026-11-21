//! WP-RTB-02.T4, the library half: the serve cache and the acknowledgement by pull, the driver
//! round, the tenant sinks, the limits, and the guard in front of the real `spot-sync-http`
//! router (requests driven in process with the client certificate in the extensions, exactly as
//! the TLS layer puts it).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use spot_core::SystemClock;
use spot_events_contract::SourceId;
use spot_sync_http::{
    AuthenticatedNode, ClientCertificate, HubSource, NodeDirectory, PushSink, StaticDirectory,
    SyncServer,
};
use spot_sync_hub_host::driver::{DriveSettings, HubCore, round};
use spot_sync_hub_host::{
    Admission, AuditThrottle, Guard, GuardSettings, Limits, RefusalAudit, ServeCache, TenantSinks,
    TokenBucket, guard_layer,
};
use spot_sync_protocol::{Batch, Hello, NodeId, Outcome, ProtocolRange, Record, RecordName};
use tower::ServiceExt as _;

const HUB_SITE: &str = "hub.test";

fn source(partition: &str) -> SourceId {
    SourceId::new(&format!("{HUB_SITE}#{partition}")).unwrap()
}

fn record(seq: u64) -> Record {
    Record::new(
        RecordName::new(&format!("bundle-{seq}")).unwrap(),
        1,
        seq,
        "{}",
    )
    .unwrap()
}

fn run_of(from: u64, to: u64) -> Vec<Record> {
    (from..=to).map(record).collect()
}

// ---- the serve cache ----------------------------------------------------------------------

#[test]
fn a_pull_reads_the_window_from_its_sequence_in_contiguous_runs() {
    let (cache, _acks) = ServeCache::new();
    cache.publish("branch_01", run_of(3, 8));
    let src = source("branch_01");
    let seqs = |records: Vec<Record>| records.iter().map(|r| r.seq).collect::<Vec<_>>();
    assert_eq!(seqs(cache.read(&src, 1, 100).unwrap()), [3, 4, 5, 6, 7, 8]);
    assert_eq!(seqs(cache.read(&src, 5, 2).unwrap()), [5, 6]);
    assert!(cache.read(&src, 9, 10).unwrap().is_empty());
    assert!(cache.read(&src, 1, 0).unwrap().is_empty());
    // A gap ends the run.
    let mut gappy = run_of(1, 3);
    gappy.push(record(5));
    cache.publish("branch_02", gappy);
    assert_eq!(
        seqs(cache.read(&source("branch_02"), 1, 10).unwrap()),
        [1, 2, 3]
    );
    // Another destination's window is not visible, and an unknown one is empty.
    assert!(cache.read(&source("nobody"), 1, 10).unwrap().is_empty());
    // A source with no partition is nobody's queue.
    assert!(
        cache
            .read(&SourceId::new(HUB_SITE).unwrap(), 1, 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_pull_for_n_is_the_acknowledgement_of_everything_below_n() {
    let (cache, mut acks) = ServeCache::new();
    cache.publish("branch_01", run_of(1, 5));
    let src = source("branch_01");
    cache.read(&src, 1, 10).unwrap();
    assert!(
        acks.try_recv().is_err(),
        "the first pull acknowledges nothing"
    );
    cache.read(&src, 4, 10).unwrap();
    let note = acks.try_recv().unwrap();
    assert_eq!((note.dest.as_str(), note.through), ("branch_01", 3));
}

#[tokio::test]
async fn a_waiting_pull_wakes_when_a_record_is_published() {
    let (cache, _acks) = ServeCache::new();
    let src = source("branch_01");
    let wait = cache.changed(&src, 1);
    let publisher = cache.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        publisher.publish("branch_01", run_of(1, 1));
    });
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("woken by the publish");
    // Level-triggered: a wait for something already published returns at once.
    tokio::time::timeout(Duration::from_secs(1), cache.changed(&src, 1))
        .await
        .expect("already published");
}

// ---- the driver round -----------------------------------------------------------------------

#[derive(Default)]
struct FakeCore {
    acks: Vec<(String, u64)>,
    ticks: usize,
    fail_tick: bool,
    windows: BTreeMap<String, Vec<Record>>,
}

impl HubCore for FakeCore {
    fn tick(&mut self, _now_ms: i64) -> Result<(), String> {
        self.ticks += 1;
        if self.fail_tick {
            Err("stream_failed".to_owned())
        } else {
            Ok(())
        }
    }
    fn window(&mut self, dest: &str, max: usize) -> Result<Vec<Record>, String> {
        Ok(self
            .windows
            .get(dest)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(max)
            .collect())
    }
    fn ack_served(&mut self, dest: &str, through: u64, _now_ms: i64) -> Result<(), String> {
        self.acks.push((dest.to_owned(), through));
        Ok(())
    }
    fn destinations(&self) -> Vec<String> {
        self.windows.keys().cloned().collect()
    }
}

#[test]
fn a_round_applies_the_newest_ack_per_destination_ticks_and_publishes() {
    let (cache, mut acks) = ServeCache::new();
    let mut core = FakeCore::default();
    core.windows.insert("branch_01".to_owned(), run_of(1, 4));
    core.windows.insert("branch_02".to_owned(), run_of(1, 2));
    cache.read(&source("branch_01"), 2, 10).unwrap();
    cache.read(&source("branch_01"), 4, 10).unwrap();
    cache.read(&source("branch_02"), 2, 10).unwrap();
    let errors = Mutex::new(Vec::<String>::new());
    let clock = SystemClock::new();
    round(
        &mut core,
        &cache,
        &mut acks,
        &clock,
        DriveSettings::default(),
        &|e| {
            errors.lock().unwrap().push(e.to_owned());
        },
    );
    core.acks.sort();
    assert_eq!(
        core.acks,
        [("branch_01".to_owned(), 3), ("branch_02".to_owned(), 1)]
    );
    assert_eq!(core.ticks, 1);
    assert_eq!(cache.depth("branch_01"), 4);
    assert_eq!(cache.depth("branch_02"), 2);
    assert!(errors.lock().unwrap().is_empty());

    // A failing tick is reported, and the windows are still published (the store may come back).
    core.fail_tick = true;
    core.windows.insert("branch_01".to_owned(), run_of(1, 6));
    round(
        &mut core,
        &cache,
        &mut acks,
        &clock,
        DriveSettings::default(),
        &|e| {
            errors.lock().unwrap().push(e.to_owned());
        },
    );
    assert_eq!(*errors.lock().unwrap(), ["stream_failed"]);
    assert_eq!(cache.depth("branch_01"), 6);
}

// ---- tenant sinks ---------------------------------------------------------------------------

struct CountingSink(Arc<AtomicUsize>);
impl PushSink for CountingSink {
    fn on_batch(&self, batch: &Batch) -> Outcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        Outcome::Ack(spot_sync_protocol::Ack::new(
            batch.source.clone(),
            batch.to_seq,
        ))
    }
}

#[test]
fn a_batch_goes_to_its_tenants_sink_and_an_unknown_tenant_applies_nothing() {
    let a = Arc::new(AtomicUsize::new(0));
    let b = Arc::new(AtomicUsize::new(0));
    let sinks = TenantSinks::new()
        .with("tenant-a.test", Arc::new(CountingSink(a.clone())))
        .with("tenant-b.test", Arc::new(CountingSink(b.clone())));
    let for_site = |site: &str| {
        Batch::new(
            SourceId::new(&format!("{site}#branch_01")).unwrap(),
            vec![record(1)],
        )
        .unwrap()
    };
    assert!(sinks.on_batch(&for_site("tenant-a.test")).is_ack());
    assert!(sinks.on_batch(&for_site("tenant-a.test")).is_ack());
    assert!(sinks.on_batch(&for_site("tenant-b.test")).is_ack());
    assert_eq!((a.load(Ordering::SeqCst), b.load(Ordering::SeqCst)), (2, 1));
    assert!(matches!(
        sinks.on_batch(&for_site("tenant-c.test")),
        Outcome::Failed(_)
    ));
    assert_eq!((a.load(Ordering::SeqCst), b.load(Ordering::SeqCst)), (2, 1));
}

// ---- limits ---------------------------------------------------------------------------------

#[test]
fn the_token_bucket_bursts_then_refills_in_whole_tokens() {
    let mut bucket = TokenBucket::new(20, 0);
    for _ in 0..20 {
        assert!(bucket.try_take(0));
    }
    assert!(!bucket.try_take(0), "the burst is the rate");
    assert!(
        !bucket.try_take(40),
        "40 ms is not a whole token at 20 a second"
    );
    assert!(bucket.try_take(50));
    assert!(!bucket.try_take(50));
    // A long pause refills to the burst, not beyond it.
    for _ in 0..20 {
        assert!(bucket.try_take(1_000_000));
    }
    assert!(!bucket.try_take(1_000_000));
    // Time going backwards never mints tokens.
    assert!(!bucket.try_take(5));
}

#[test]
fn admission_holds_per_node_and_global_slots_until_the_permit_drops() {
    let admission = Admission::new(Limits {
        per_node_connections: 2,
        global_connections: 3,
        ..Limits::default()
    });
    let a1 = admission.enter("a", 0).unwrap();
    let _a2 = admission.enter("a", 0).unwrap();
    assert_eq!(
        admission.enter("a", 0).unwrap_err().word(),
        "per_node_limit"
    );
    let _b1 = admission.enter("b", 0).unwrap();
    assert_eq!(admission.enter("b", 0).unwrap_err().word(), "global_limit");
    assert_eq!(admission.in_flight(), 3);
    drop(a1);
    assert_eq!(admission.in_flight(), 2);
    assert!(admission.enter("c", 0).is_ok());
}

#[test]
fn audit_is_at_most_once_per_key_per_interval() {
    let throttle = AuditThrottle::new(60_000);
    assert!(throttle.should_audit("node-a", 0));
    assert!(!throttle.should_audit("node-a", 59_999));
    assert!(throttle.should_audit("node-b", 100));
    assert!(throttle.should_audit("node-a", 60_000));
}

// ---- the guard over the real router -----------------------------------------------------------

#[derive(Default)]
struct Audit(Mutex<Vec<(String, String)>>);
impl RefusalAudit for Audit {
    fn refused(&self, who: &str, reason: &str) {
        self.0
            .lock()
            .unwrap()
            .push((who.to_owned(), reason.to_owned()));
    }
}

fn cert(seed: u8) -> ClientCertificate {
    ClientCertificate::from_der(vec![0x30, 0x82, seed, seed, seed])
}

struct Rig {
    router: axum::Router,
    guard: Arc<Guard>,
    audit: Arc<Audit>,
    cache: Arc<ServeCache>,
    pushed: Arc<AtomicUsize>,
}

fn rig(limits: Limits) -> Rig {
    let mut directory = StaticDirectory::new();
    // Two nodes of the same tenant, one of another tenant.
    directory.enroll(
        cert(1).fingerprint(),
        NodeId::new("hub.test#branch_01").unwrap(),
        ["branch_01".to_owned()],
    );
    directory.enroll(
        cert(2).fingerprint(),
        NodeId::new("hub.test#branch_02").unwrap(),
        ["branch_02".to_owned()],
    );
    directory.enroll(
        cert(3).fingerprint(),
        NodeId::new("other.test#branch_01").unwrap(),
        ["branch_01".to_owned()],
    );
    let directory: Arc<dyn NodeDirectory> = Arc::new(directory);
    let (cache, _acks) = ServeCache::new();
    cache.publish("branch_01", run_of(1, 3));
    let pushed = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(SystemClock::new());
    let server = SyncServer::new(
        NodeId::new("hub.test#hub_main").unwrap(),
        Arc::clone(&directory),
        cache.clone(),
        Arc::new(CountingSink(pushed.clone())),
        clock.clone(),
    )
    .with_settings(spot_sync_http::ServerSettings {
        long_poll: Duration::from_millis(60),
        heartbeat: Duration::from_millis(20),
        ..spot_sync_http::ServerSettings::default()
    });
    let audit = Arc::new(Audit::default());
    let guard = Guard::new(
        GuardSettings {
            limits,
            hub_site: HUB_SITE.to_owned(),
        },
        directory,
        clock,
        audit.clone(),
        None,
    );
    Rig {
        router: guard_layer(server.router(), guard.clone()),
        guard,
        audit,
        cache,
        pushed,
    }
}

fn request(
    method: &str,
    uri: &str,
    certificate: Option<ClientCertificate>,
    body: Body,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(certificate) = certificate {
        builder = builder.extension(certificate);
    }
    builder.body(body).unwrap()
}

fn pull(partition: &str, certificate: Option<ClientCertificate>) -> Request<Body> {
    request(
        "GET",
        &format!("/sync/v1/pull?source={HUB_SITE}%23{partition}&from_seq=1&max=10"),
        certificate,
        Body::empty(),
    )
}

async fn status(rig: &Rig, request: Request<Body>) -> StatusCode {
    rig.router.clone().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn a_node_pulls_its_own_queue_and_never_another_nodes() {
    let rig = rig(Limits::default());
    assert_eq!(
        status(&rig, pull("branch_01", Some(cert(1)))).await,
        StatusCode::OK
    );
    // branch_02's certificate naming branch_01's queue, and the reverse.
    assert_eq!(
        status(&rig, pull("branch_01", Some(cert(2)))).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(&rig, pull("branch_02", Some(cert(1)))).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rig.guard.counts().get("sync_http.partition_violation"),
        Some(&2)
    );
    let audited = rig.audit.0.lock().unwrap().clone();
    assert_eq!(
        audited.len(),
        2,
        "two different nodes, one record each: {audited:?}"
    );
}

#[tokio::test]
async fn an_unknown_certificate_and_a_missing_one_never_reach_a_handler() {
    let rig = rig(Limits::default());
    assert_eq!(
        status(&rig, pull("branch_01", Some(cert(9)))).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(&rig, pull("branch_01", None)).await,
        StatusCode::UNAUTHORIZED
    );
    // A flood of unknown certificates is counted every time and audited once.
    for _ in 0..20 {
        assert_eq!(
            status(&rig, pull("branch_01", Some(cert(9)))).await,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        rig.guard.counts().get("sync_http.identity_unknown"),
        Some(&21)
    );
    assert_eq!(rig.audit.0.lock().unwrap().len(), 1);
    assert_eq!(rig.pushed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_hello_naming_another_tenant_is_refused() {
    let rig = rig(Limits::default());
    let hello = |node: &str| {
        serde_json::to_vec(&Hello::new(
            NodeId::new(node).unwrap(),
            ProtocolRange::current(),
            0,
        ))
        .unwrap()
    };
    let own = request(
        "POST",
        "/sync/v1/hello",
        Some(cert(1)),
        Body::from(hello("hub.test#branch_01")),
    );
    assert_eq!(status(&rig, own).await, StatusCode::OK);
    // cert 3 is pinned for other.test; its hello names hub.test's node.
    let foreign = request(
        "POST",
        "/sync/v1/hello",
        Some(cert(3)),
        Body::from(hello("hub.test#branch_01")),
    );
    assert_eq!(status(&rig, foreign).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn pushes_past_the_rate_are_told_to_wait_and_nothing_of_them_is_applied() {
    let rig = rig(Limits {
        push_per_second: 3,
        ..Limits::default()
    });
    let push = || {
        request(
            "POST",
            "/sync/v1/push",
            Some(cert(1)),
            Body::from("not a batch"),
        )
    };
    for _ in 0..3 {
        // Admitted (the body is not a batch, so the router answers 400), not rate limited.
        assert_eq!(status(&rig, push()).await, StatusCode::BAD_REQUEST);
    }
    assert_eq!(status(&rig, push()).await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rig.guard.counts().get("push_rate"), Some(&1));
    assert_eq!(rig.pushed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_long_pull_keeps_its_slot_until_its_body_ends() {
    let rig = rig(Limits {
        per_node_connections: 1,
        ..Limits::default()
    });
    let first = rig
        .router
        .clone()
        .oneshot(pull("branch_01", Some(cert(1))))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        rig.guard.in_flight(),
        1,
        "the slot is held while the body is open"
    );
    assert_eq!(
        status(&rig, pull("branch_01", Some(cert(1)))).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    // Another node is unaffected.
    assert_eq!(
        status(&rig, pull("branch_02", Some(cert(2)))).await,
        StatusCode::OK
    );
    drop(first);
    assert_eq!(rig.guard.in_flight(), 0);
    assert_eq!(
        status(&rig, pull("branch_01", Some(cert(1)))).await,
        StatusCode::OK
    );
    assert_eq!(rig.cache.depth("branch_01"), 3);
}

#[tokio::test]
async fn the_global_ceiling_refuses_everyone_past_it() {
    let rig = rig(Limits {
        global_connections: 1,
        per_node_connections: 2,
        ..Limits::default()
    });
    let held = rig
        .router
        .clone()
        .oneshot(pull("branch_01", Some(cert(1))))
        .await
        .unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    assert_eq!(
        status(&rig, pull("branch_02", Some(cert(2)))).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(held);
    assert_eq!(
        status(&rig, pull("branch_02", Some(cert(2)))).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_node_revoked_while_connected_is_refused_at_its_next_request() {
    // The directory is live: revoke by building a second guard over a revoked entry.
    let mut directory = StaticDirectory::new();
    directory.enroll(
        cert(1).fingerprint(),
        NodeId::new("hub.test#branch_01").unwrap(),
        ["branch_01".to_owned()],
    );
    directory.revoke(cert(1).fingerprint());
    let directory: Arc<dyn NodeDirectory> = Arc::new(directory);
    let (cache, _acks) = ServeCache::new();
    let clock = Arc::new(SystemClock::new());
    let server = SyncServer::new(
        NodeId::new("hub.test#hub_main").unwrap(),
        Arc::clone(&directory),
        cache,
        Arc::new(CountingSink(Arc::default())),
        clock.clone(),
    );
    let guard = Guard::new(
        GuardSettings {
            limits: Limits::default(),
            hub_site: HUB_SITE.to_owned(),
        },
        directory,
        clock,
        Arc::new(Audit::default()),
        None,
    );
    let router = guard_layer(server.router(), guard.clone());
    let response = router
        .oneshot(pull("branch_01", Some(cert(1))))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(guard.counts().get("sync_http.identity_revoked"), Some(&1));
    let _ = AuthenticatedNode::new(
        NodeId::new("hub.test#branch_01").unwrap(),
        ["branch_01".to_owned()],
    );
}
