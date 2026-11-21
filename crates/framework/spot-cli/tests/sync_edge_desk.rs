//! The sync hub's edge desk (WP-AI-07.T9): a lagging edge shows its expected position above its
//! actual one; a resync asks the catch-up path and, once the edge has pulled and the driver has
//! trimmed its queue, the positions are equal; an unknown and a revoked edge are refused; a second
//! site's edges are invisible; a desk with no catch-up path answers `not supported`; the remedy
//! host's edge part works over the provided port (rehearsal and live). The relay core is an
//! in-memory queue driven by the real driver round, so the cache and the acknowledgements are the
//! hub's own.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_cli::services::SiteCtx;
use spot_cli::services::sync_hub::edge_desk::{
    CATCHUP_REFUSED, Catchup, DirectoryRoster, EdgeLink, EdgeRoster, EdgeStatus, HubEdgeDesk,
    NOT_SUPPORTED, REVOKED_EDGE, RelayRoster, UNKNOWN_EDGE, UNREADABLE,
};
use spot_cli::services::sync_hub::{self, ROLES};
use spot_cli::services::world_access::edge::{EdgeDesk, EdgeState, from_parts};
use spot_core::ManualClock;
use spot_remedies::EdgeRef;
use spot_sync_http::HubSource;
use spot_sync_hub_host::ServeCache;
use spot_sync_hub_host::cache::AckNote;
use spot_sync_hub_host::driver::{DriveSettings, HubCore, round};
use spot_sync_protocol::{Record, RecordName, SourceId};
use time::OffsetDateTime;
use tokio::sync::mpsc::UnboundedReceiver;

/// An in-memory relay core: the queued serve sequences of each destination.
#[derive(Clone, Default)]
struct Queues(Arc<Mutex<BTreeMap<String, Vec<u64>>>>);

impl Queues {
    fn set(&self, dest: &str, seqs: &[u64]) {
        self.0
            .lock()
            .unwrap()
            .insert(dest.to_owned(), seqs.to_vec());
    }
}

impl HubCore for Queues {
    fn tick(&mut self, _now_ms: i64) -> Result<(), String> {
        Ok(())
    }

    fn window(&mut self, dest: &str, max: usize) -> Result<Vec<Record>, String> {
        let queues = self.0.lock().unwrap();
        let seqs = queues.get(dest).cloned().unwrap_or_default();
        Ok(seqs
            .into_iter()
            .take(max)
            .map(|seq| {
                Record::new(
                    RecordName::new(&format!("bundle-{seq}")).unwrap(),
                    1,
                    seq,
                    "{}".to_owned(),
                )
                .unwrap()
            })
            .collect())
    }

    fn ack_served(&mut self, dest: &str, through: u64, _now_ms: i64) -> Result<(), String> {
        if let Some(queue) = self.0.lock().unwrap().get_mut(dest) {
            queue.retain(|seq| *seq > through);
        }
        Ok(())
    }

    fn destinations(&self) -> Vec<String> {
        self.0.lock().unwrap().keys().cloned().collect()
    }
}

/// One hub: its cache, driver state, queues and desk.
struct Hub {
    cache: Arc<ServeCache>,
    acks: UnboundedReceiver<AckNote>,
    core: Queues,
    clock: ManualClock,
    desk: Arc<HubEdgeDesk>,
    site: String,
}

impl Hub {
    fn step(&mut self) {
        let mut core = self.core.clone();
        round(
            &mut core,
            &self.cache,
            &mut self.acks,
            &self.clock,
            DriveSettings::default(),
            &|reason| panic!("the relay round failed: {reason}"),
        );
    }

    /// The edge pulls everything it has and asks for the next sequence (the acknowledgement).
    fn pull_through(&self, edge: &str, through: u64) {
        let source = SourceId::new(&format!("{}#{edge}", self.site)).unwrap();
        let _ = self.cache.read(&source, through + 1, 100).unwrap();
    }
}

struct FixedRoster(Vec<EdgeStatus>);

impl EdgeRoster for FixedRoster {
    fn enrolled(&self) -> Result<Vec<EdgeStatus>, &'static str> {
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct Calls(Mutex<Vec<String>>);

impl Catchup for Calls {
    fn restart(&self, edge: &str) -> Result<(), &'static str> {
        self.0.lock().unwrap().push(edge.to_owned());
        Ok(())
    }
}

struct Refusing;

impl Catchup for Refusing {
    fn restart(&self, _edge: &str) -> Result<(), &'static str> {
        Err(CATCHUP_REFUSED)
    }
}

fn status(edge: &str, revoked: bool) -> EdgeStatus {
    EdgeStatus {
        edge: edge.to_owned(),
        revoked,
    }
}

fn hub(
    site: &str,
    queues: &[(&str, &[u64])],
    roster: Option<Vec<EdgeStatus>>,
    catchup: Option<Arc<dyn Catchup>>,
) -> Hub {
    let (cache, acks) = ServeCache::new();
    let link = Arc::new(EdgeLink::default());
    link.attach(Arc::clone(&cache));
    let core = Queues::default();
    for (dest, seqs) in queues {
        core.set(dest, seqs);
    }
    let roster: Arc<dyn EdgeRoster> = match roster {
        Some(entries) => Arc::new(FixedRoster(entries)),
        None => Arc::new(RelayRoster::new(Arc::clone(&link))),
    };
    let desk = Arc::new(HubEdgeDesk::new(site, link, roster, catchup));
    let mut hub = Hub {
        cache,
        acks,
        core,
        clock: ManualClock::new(OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()),
        desk,
        site: site.to_owned(),
    };
    hub.step();
    hub
}

fn state(edge: &str, expected: u64, actual: u64) -> EdgeState {
    EdgeState {
        edge: edge.to_owned(),
        expected,
        actual,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lagging_edge_shows_its_head_above_its_position_and_resync_catches_up() {
    let calls = Arc::new(Calls::default());
    let mut hub = hub(
        "acme",
        &[("customer_1", &[8, 9, 10]), ("customer_2", &[])],
        None,
        Some(Arc::clone(&calls) as Arc<dyn Catchup>),
    );
    // Items 8 to 10 wait on customer_1: it acknowledged through 7 and the head is 10.
    let before = hub.desk.edges().await.unwrap();
    assert_eq!(
        before,
        vec![state("customer_1", 10, 7), state("customer_2", 0, 0)]
    );
    // The resync asks the catch-up path for that edge and answers its state.
    let asked = hub.desk.resync("customer_1").await.unwrap();
    assert_eq!(*calls.0.lock().unwrap(), vec!["customer_1".to_owned()]);
    assert_eq!(asked.edge, "customer_1");
    // Asking twice is asking once as far as the relay is concerned: nothing was removed.
    assert_eq!(hub.desk.resync("customer_1").await.unwrap(), asked);
    assert_eq!(hub.core.0.lock().unwrap()["customer_1"], vec![8, 9, 10]);
    // The catch-up is served: the edge pulls from 11, which acknowledges through 10, and the
    // driver trims the queue.
    hub.pull_through("customer_1", 10);
    hub.step();
    let after = hub.desk.edges().await.unwrap();
    assert_eq!(
        after,
        vec![state("customer_1", 10, 10), state("customer_2", 0, 0)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_and_a_revoked_edge_are_refused_and_never_reach_the_catchup_path() {
    let calls = Arc::new(Calls::default());
    let hub = hub(
        "acme",
        &[("customer_1", &[3]), ("old_branch", &[4, 5])],
        Some(vec![
            status("customer_1", false),
            status("old_branch", true),
        ]),
        Some(Arc::clone(&calls) as Arc<dyn Catchup>),
    );
    // A revoked edge is not listed.
    let listed = hub.desk.edges().await.unwrap();
    assert_eq!(listed, vec![state("customer_1", 3, 2)]);
    assert_eq!(hub.desk.resync("nobody").await.unwrap_err(), UNKNOWN_EDGE);
    assert_eq!(
        hub.desk.resync("old_branch").await.unwrap_err(),
        REVOKED_EDGE
    );
    assert!(calls.0.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sites_edges_are_invisible() {
    let calls = Arc::new(Calls::default());
    let acme = hub(
        "acme",
        &[("customer_1", &[2])],
        None,
        Some(Arc::clone(&calls) as Arc<dyn Catchup>),
    );
    let globex = hub("globex", &[("branch_9", &[5, 6])], None, None);
    assert_eq!(
        acme.desk.edges().await.unwrap(),
        vec![state("customer_1", 2, 1)]
    );
    assert_eq!(
        globex.desk.edges().await.unwrap(),
        vec![state("branch_9", 6, 4)]
    );
    assert_eq!(
        acme.desk.resync("branch_9").await.unwrap_err(),
        UNKNOWN_EDGE
    );
    assert_eq!(
        globex.desk.resync("customer_1").await.unwrap_err(),
        UNKNOWN_EDGE
    );
    assert!(calls.0.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_desk_with_no_catchup_path_or_a_refusing_one_or_no_running_hub_refuses_with_a_fixed_reason()
 {
    let none = hub("acme", &[("customer_1", &[2])], None, None);
    assert_eq!(
        none.desk.resync("customer_1").await.unwrap_err(),
        NOT_SUPPORTED
    );
    // The listing still works without a catch-up path.
    assert_eq!(none.desk.edges().await.unwrap().len(), 1);

    let refusing = hub(
        "acme",
        &[("customer_1", &[2])],
        None,
        Some(Arc::new(Refusing) as Arc<dyn Catchup>),
    );
    assert_eq!(
        refusing.desk.resync("customer_1").await.unwrap_err(),
        CATCHUP_REFUSED
    );

    // A hub that is not running (its cache is gone) cannot be read.
    let link = Arc::new(EdgeLink::default());
    let stopped = HubEdgeDesk::new(
        "acme",
        Arc::clone(&link),
        Arc::new(RelayRoster::new(link)),
        None,
    );
    assert_eq!(stopped.edges().await.unwrap_err(), UNREADABLE);
    assert_eq!(stopped.resync("customer_1").await.unwrap_err(), UNREADABLE);
}

async fn blocking<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(work).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_remedy_hosts_edge_part_runs_over_the_desk_dry_run_and_apply() {
    let calls = Arc::new(Calls::default());
    let hub = hub(
        "acme",
        &[("customer_1", &[8, 9, 10]), ("customer_2", &[])],
        None,
        Some(Arc::clone(&calls) as Arc<dyn Catchup>),
    );
    let part = from_parts(
        Some(Arc::clone(&hub.desk) as Arc<dyn EdgeDesk>),
        Some(tokio::runtime::Handle::current()),
    )
    .expect("a part over the desk");
    let (drifted, dry, applied, calls_after_dry) = {
        let calls = Arc::clone(&calls);
        blocking(move || {
            let drifted = part.live().drifted();
            // Dry run: the rehearsal copy is repaired, the hub's catch-up path is not asked.
            let dry = part
                .rehearsal()
                .unwrap()
                .repair(&EdgeRef::new("customer_1"))
                .unwrap();
            let calls_after_dry = calls.0.lock().unwrap().len();
            // Apply: the live ops ask the catch-up path.
            let applied = part.live().repair(&EdgeRef::new("customer_1")).unwrap();
            (drifted, dry, applied, calls_after_dry)
        })
        .await
    };
    assert_eq!(drifted, vec![EdgeRef::new("customer_1")]);
    assert_eq!(dry.before, 7);
    assert_eq!(calls_after_dry, 0, "a dry run never reaches the hub");
    assert_eq!(applied.before, 7);
    assert_eq!(*calls.0.lock().unwrap(), vec!["customer_1".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_hub_site_provides_no_desk() {
    // The module runs in the hub role only; a context nobody registered the hub into has no desk.
    assert_eq!(ROLES, &["hub"]);
    let ctx = SiteCtx::new(
        "acme",
        std::time::Duration::from_secs(1),
        Arc::new(ManualClock::new(
            OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
        )),
        BTreeMap::new(),
        true,
    );
    assert!(ctx.port::<dyn EdgeDesk>().is_none());
    assert!(sync_hub::edge_desk::recall("a-site-that-never-provided-one").is_none());
    // And the part is absent without a desk.
    assert!(from_parts(None, Some(tokio::runtime::Handle::current())).is_none());
}

// ---- the roster over the enrolment directory, and the exact head (WP-AI-07.T16) -----------------

mod directory {
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

    use spot_sync_enrol::{
        DirectoryConfig, DurableDirectory, EnrolRequest, EnrolService, EnrolmentStore, Ids,
        MemoryEnrolmentStore, NodeIdentity, RandomSource,
    };
    use spot_sync_protocol::NodeId;

    use super::*;

    const T0: i64 = 1_767_225_600;

    pub struct StepClock(pub AtomicI64);

    impl spot_sync_enrol::Clock for StepClock {
        fn now(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct CountingRandom(AtomicU64);

    impl RandomSource for CountingRandom {
        fn fill(&self, buffer: &mut [u8]) {
            for byte in buffer {
                let n = self.0.fetch_add(1, Ordering::SeqCst);
                *byte = (n.wrapping_mul(2_654_435_761) >> 7) as u8;
            }
        }
    }

    #[derive(Default)]
    struct SeqIds(AtomicU64);

    impl Ids for SeqIds {
        fn uuid(&self) -> String {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            format!("00000000-0000-4000-8000-{n:012x}")
        }
        fn enrolment_id(&self) -> String {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            format!("enrol_{n}")
        }
    }

    /// One hub process: a directory and an enrolment service over a (possibly shared) store.
    pub struct Process {
        pub directory: Arc<DurableDirectory>,
        pub service: EnrolService,
    }

    pub fn clock() -> Arc<StepClock> {
        Arc::new(StepClock(AtomicI64::new(T0)))
    }

    pub fn store() -> Arc<dyn EnrolmentStore> {
        Arc::new(MemoryEnrolmentStore::new())
    }

    pub fn process(store: &Arc<dyn EnrolmentStore>, clock: &Arc<StepClock>) -> Process {
        let directory = Arc::new(DurableDirectory::new(
            Arc::clone(store),
            Arc::clone(clock) as Arc<dyn spot_sync_enrol::Clock>,
            DirectoryConfig::default(),
        ));
        directory.refresh().unwrap();
        let service = EnrolService::new(
            Arc::clone(store),
            Arc::clone(&directory),
            Arc::new(CountingRandom::default()),
            Arc::new(SeqIds::default()),
            Arc::clone(clock) as Arc<dyn spot_sync_enrol::Clock>,
        );
        Process { directory, service }
    }

    pub fn node(id: &str) -> NodeId {
        NodeId::new(id).unwrap()
    }

    /// Enrols `id` (`<site>#<partition>`) end to end through `process`.
    pub fn enrol(process: &Process, id: &str, partition: &str) {
        let request = EnrolRequest {
            node: node(id),
            role: "edge".to_owned(),
            partitions: vec![partition.to_owned()],
            template: "{}".to_owned(),
            ttl_seconds: None,
        };
        let issued = process.service.create_enrolment("admin", &request).unwrap();
        let identity = NodeIdentity::generate_replacement(&node(id), T0).unwrap();
        process
            .service
            .redeem(
                &issued.code,
                &node(id),
                identity.certificate_der(),
                "source",
            )
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_enrolment_is_listed_revoked_and_is_not_resynced() {
    let clock = directory::clock();
    let store = directory::store();
    let process = directory::process(&store, &clock);
    directory::enrol(&process, "edge-khi#customer_1", "customer_1");
    directory::enrol(&process, "edge-lhe#old_branch", "old_branch");
    process
        .service
        .revoke("admin", &directory::node("edge-lhe#old_branch"))
        .unwrap();
    let roster = DirectoryRoster::new(Arc::clone(&process.directory));
    let mut listed = roster.enrolled().unwrap();
    listed.sort_by(|a, b| a.edge.cmp(&b.edge));
    assert_eq!(
        listed,
        vec![status("customer_1", false), status("old_branch", true)]
    );

    // The desk over the directory roster: the revoked edge is not shown and is never resynced.
    let calls = Arc::new(Calls::default());
    let core = Queues::default();
    core.set("customer_1", &[3]);
    core.set("old_branch", &[4, 5]);
    let (cache, mut acks) = ServeCache::new();
    let link = Arc::new(EdgeLink::default());
    link.attach(Arc::clone(&cache));
    let mut driven = core.clone();
    round(
        &mut driven,
        &cache,
        &mut acks,
        &ManualClock::new(OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()),
        DriveSettings::default(),
        &|reason| panic!("the relay round failed: {reason}"),
    );
    let desk = HubEdgeDesk::new(
        "acme",
        link,
        Arc::new(roster),
        Some(Arc::clone(&calls) as Arc<dyn Catchup>),
    );
    assert_eq!(desk.edges().await.unwrap(), vec![state("customer_1", 3, 2)]);
    assert_eq!(desk.resync("old_branch").await.unwrap_err(), REVOKED_EDGE);
    assert_eq!(desk.resync("nobody").await.unwrap_err(), UNKNOWN_EDGE);
    assert!(calls.0.lock().unwrap().is_empty());
    // The live edge is resynced under its node id: the name the catch-up path knows it by.
    desk.resync("customer_1").await.unwrap();
    assert_eq!(
        *calls.0.lock().unwrap(),
        vec!["edge-khi#customer_1".to_owned()]
    );
}

#[test]
fn a_new_enrolment_and_a_revocation_made_elsewhere_show_after_a_refresh() {
    let clock = directory::clock();
    let store = directory::store();
    let ours = directory::process(&store, &clock);
    // Another process of the same hub enrols and revokes through the shared store.
    let other = directory::process(&store, &clock);
    let roster = DirectoryRoster::new(Arc::clone(&ours.directory));
    assert_eq!(roster.enrolled().unwrap(), vec![]);

    directory::enrol(&other, "edge-khi#customer_1", "customer_1");
    assert_eq!(
        roster.enrolled().unwrap(),
        vec![],
        "this process has not refreshed yet"
    );
    ours.directory.refresh().unwrap();
    assert_eq!(
        roster.enrolled().unwrap(),
        vec![status("customer_1", false)]
    );

    other
        .service
        .revoke("admin", &directory::node("edge-khi#customer_1"))
        .unwrap();
    assert_eq!(
        roster.enrolled().unwrap(),
        vec![status("customer_1", false)],
        "the revocation is not seen before the next refresh"
    );
    ours.directory.refresh().unwrap();
    assert_eq!(roster.enrolled().unwrap(), vec![status("customer_1", true)]);
}

#[test]
fn a_directory_too_old_to_trust_is_unreadable_not_empty() {
    let clock = directory::clock();
    let store = directory::store();
    let process = directory::process(&store, &clock);
    directory::enrol(&process, "edge-khi#customer_1", "customer_1");
    let roster = DirectoryRoster::new(Arc::clone(&process.directory));
    assert_eq!(roster.enrolled().unwrap().len(), 1);
    // The directory refuses everyone after 30 seconds without a refresh; so does the roster.
    clock.0.fetch_add(31, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(roster.enrolled().unwrap_err(), UNREADABLE);
    process.directory.refresh().unwrap();
    assert_eq!(roster.enrolled().unwrap().len(), 1);
}

/// A core that reports the head of each queue (the newest serve sequence assigned), as the relay
/// core does through `SendQueue::last_serve_seq`.
struct HeadQueues(Queues, BTreeMap<String, u64>);

impl HubCore for HeadQueues {
    fn tick(&mut self, now_ms: i64) -> Result<(), String> {
        self.0.tick(now_ms)
    }
    fn window(&mut self, dest: &str, max: usize) -> Result<Vec<Record>, String> {
        self.0.window(dest, max)
    }
    fn ack_served(&mut self, dest: &str, through: u64, now_ms: i64) -> Result<(), String> {
        self.0.ack_served(dest, through, now_ms)
    }
    fn destinations(&self) -> Vec<String> {
        self.0.destinations()
    }
    fn head_serve(&mut self, dest: &str) -> Option<u64> {
        self.1.get(dest).copied()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expected_is_the_relays_queue_head_beyond_the_published_window() {
    // Twenty items wait (11 to 30), but the driver publishes a window of three.
    let queues = Queues::default();
    let seqs: Vec<u64> = (11..=30).collect();
    queues.set("customer_1", &seqs);
    let settings = DriveSettings {
        window: 3,
        ..DriveSettings::default()
    };
    let clock = ManualClock::new(OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap());
    let desk_over = |core: &mut dyn FnMut(&Arc<ServeCache>, &mut UnboundedReceiver<AckNote>)| {
        let (cache, mut acks) = ServeCache::new();
        core(&cache, &mut acks);
        let link = Arc::new(EdgeLink::default());
        link.attach(Arc::clone(&cache));
        HubEdgeDesk::new(
            "acme",
            Arc::clone(&link),
            Arc::new(FixedRoster(vec![status("customer_1", false)])),
            None,
        )
    };

    // With the head reported: exact. Acknowledged through 10, head 30.
    let with_head = desk_over(&mut |cache, acks| {
        let mut core = HeadQueues(
            queues.clone(),
            BTreeMap::from([("customer_1".to_owned(), 30)]),
        );
        round(&mut core, cache, acks, &clock, settings, &|reason| {
            panic!("the relay round failed: {reason}")
        });
    });
    assert_eq!(
        with_head.edges().await.unwrap(),
        vec![state("customer_1", 30, 10)]
    );

    // Without it: the old reading, which only sees the window (13).
    let without = desk_over(&mut |cache, acks| {
        let mut core = queues.clone();
        round(&mut core, cache, acks, &clock, settings, &|reason| {
            panic!("the relay round failed: {reason}")
        });
    });
    assert_eq!(
        without.edges().await.unwrap(),
        vec![state("customer_1", 13, 10)]
    );
}
