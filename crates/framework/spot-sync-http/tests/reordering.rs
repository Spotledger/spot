//! Reordered, stale and duplicate responses through the real handlers and the real client
//! (WP-SYN-11.T4, chaos assumption 1 of `docs/research/sync-chaos-assumptions.md`).
//!
//! The HTTP binding is ordered over protocol records: the receiver refuses a gap
//! (`Nack::Gap`, nothing applied) and acknowledges a batch entirely below its checkpoint without
//! applying it. This suite proves that with a [`ReorderingTransport`], a deterministic model of a
//! network that delivers a genuine response to the wrong request. The double never edits a body:
//! every byte the client decodes came from the hub, so the binding's own decode and `verify` path
//! runs on every delivery (tampering is `heartbeat_time.rs`'s job, not this file's).
//!
//! Assertion messages and the action log carry sequence numbers and action names only.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(missing_docs)]

mod support;

use std::sync::{Arc, Mutex};

use support::*;

use spot_core::SystemClock;
use spot_sync_http::server::{PULL_PATH, PUSH_PATH, pull_query};
use spot_sync_http::{
    BoxFuture, ClientCertificate, EdgeClient, Error, HttpRequest, HttpResponse, LoopbackTransport,
    MemoryHub, NoJitter, NoopSleeper, PullStep, PushStep, SharedReceiver, Transport,
};
use spot_sync_protocol::{
    ApplySink, MemoryCheckpoints, MemorySink, Nack, NodeId, Offer, Receiver, ReceiverConfig,
    Record, RecordKey, SourceId,
};

/// What the double does with one request, chosen by the request's position in the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Forward the request and deliver its own response.
    Pass,
    /// Forward the request, keep the response and fail the call with a retryable transport
    /// error, so the caller believes the exchange was lost.
    Hold,
    /// Do not forward: answer with the n-th held response, a stale response arriving late.
    ReleaseHeld(usize),
    /// Pull only: answer with the response the hub gives to a pull from `from_seq`, an
    /// out-of-order response.
    Replace(u64),
}

/// The state the double shares with the test that owns it.
#[derive(Default)]
struct Book {
    /// How many requests the double has seen.
    seen: usize,
    /// The responses kept by `Hold`, in order.
    held: Vec<HttpResponse>,
    /// What the double did, one entry per request: `"<n>:<action>"`.
    log: Vec<String>,
    /// How many push requests were forwarded to the hub (so a Replace/Release is not counted).
    forwarded_pushes: usize,
}

/// A transport that delivers genuine responses in the wrong order, from a plan.
///
/// A plan is data: request number `n` gets `plan[n]`, and every request past the end of the plan
/// is passed through. There is no clock and no randomness.
struct ReorderingTransport {
    inner: Arc<dyn Transport>,
    source: SourceId,
    max: usize,
    plan: Vec<Action>,
    book: Arc<Mutex<Book>>,
}

impl ReorderingTransport {
    /// A double over `inner`. `source` and `max` rebuild the query for `Replace`.
    fn new(
        inner: Arc<dyn Transport>,
        source: &SourceId,
        max: usize,
        plan: Vec<Action>,
    ) -> (Arc<Self>, Arc<Mutex<Book>>) {
        let book = Arc::new(Mutex::new(Book::default()));
        let transport = Arc::new(Self {
            inner,
            source: source.clone(),
            max,
            plan,
            book: Arc::clone(&book),
        });
        (transport, book)
    }
}

/// The actions taken so far, for an assertion message.
fn log(book: &Arc<Mutex<Book>>) -> String {
    book.lock().unwrap().log.join(" ")
}

impl Transport for ReorderingTransport {
    fn request(&self, request: HttpRequest) -> BoxFuture<Result<HttpResponse, Error>> {
        let (n, action) = {
            let mut book = self.book.lock().unwrap();
            let n = book.seen;
            book.seen += 1;
            let action = self.plan.get(n).copied().unwrap_or(Action::Pass);
            book.log.push(format!("{n}:{action:?}"));
            (n, action)
        };
        let inner = Arc::clone(&self.inner);
        let book = Arc::clone(&self.book);
        let source = self.source.clone();
        let max = self.max;
        let is_push = request.path == PUSH_PATH;
        Box::pin(async move {
            match action {
                Action::Pass => {
                    if is_push {
                        book.lock().unwrap().forwarded_pushes += 1;
                    }
                    inner.request(request).await
                }
                Action::Hold => {
                    if is_push {
                        book.lock().unwrap().forwarded_pushes += 1;
                    }
                    let response = inner.request(request).await?;
                    book.lock().unwrap().held.push(response);
                    Err(Error::unavailable(format!("request {n} was held")))
                }
                Action::ReleaseHeld(index) => {
                    let held = book.lock().unwrap().held.get(index).cloned();
                    held.ok_or_else(|| {
                        Error::invalid_request(format!("request {n}: no held response {index}"))
                    })
                }
                Action::Replace(from_seq) => {
                    assert_eq!(request.path, PULL_PATH, "request {n}: Replace is for pulls");
                    let other =
                        HttpRequest::get(PULL_PATH).with_query(pull_query(&source, from_seq, max));
                    inner.request(other).await
                }
            }
        })
    }
}

/// A sink that counts every `apply` call it gets, including the ones that change nothing.
#[derive(Default)]
struct CountingSink {
    inner: MemorySink,
    /// The sequence of every `apply` call, in call order.
    calls: Vec<u64>,
}

impl ApplySink for CountingSink {
    fn applied(&self, source: &SourceId, key: &RecordKey) -> bool {
        self.inner.applied(source, key)
    }

    fn apply(&mut self, source: &SourceId, record: &Record) -> spot_sync_protocol::Result<()> {
        self.calls.push(record.seq);
        self.inner.apply(source, record)
    }
}

type CountingReceiver = Receiver<CountingSink, MemoryCheckpoints>;

fn counting_receiver() -> CountingReceiver {
    Receiver::new(
        ReceiverConfig::default(),
        CountingSink::default(),
        MemoryCheckpoints::new(),
    )
    .unwrap()
}

fn checkpoint(receiver: &CountingReceiver, source: &SourceId) -> u64 {
    receiver
        .stored_checkpoint(source)
        .map_or(0, |checkpoint| checkpoint.last_applied_seq)
}

/// Every `apply` call was for a new sequence, in strictly increasing order: nothing twice,
/// nothing out of order.
fn strictly_increasing(calls: &[u64]) -> bool {
    calls.windows(2).all(|pair| pair[0] < pair[1])
}

/// A hub that serves `count` records of one source, and a client whose transport is the double.
struct Pull {
    source: SourceId,
    client: EdgeClient,
    book: Arc<Mutex<Book>>,
}

fn pull_rig(count: u64, plan: Vec<Action>) -> Pull {
    let source = SourceId::new("head-office#hub").unwrap();
    let stream = Arc::new(MemoryHub::new());
    stream.append(&source, records(&source, 1, count));
    let certificate = certificate(41);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let hub = hub(
        Arc::new(directory(&certificate, &node, &["branch_01"])),
        stream,
        Arc::new(SharedReceiver::new(receiver())),
        settings(),
    );
    let (client, book) = reordering_client(&hub, &node, certificate, &source, 10, plan);
    Pull {
        source,
        client,
        book,
    }
}

fn reordering_client(
    hub: &Hub,
    node: &NodeId,
    certificate: ClientCertificate,
    source: &SourceId,
    max: usize,
    plan: Vec<Action>,
) -> (EdgeClient, Arc<Mutex<Book>>) {
    let inner: Arc<dyn Transport> =
        Arc::new(LoopbackTransport::new(hub.router.clone(), certificate));
    let (transport, book) = ReorderingTransport::new(inner, source, max, plan);
    let client = EdgeClient::new(
        transport,
        identity(node),
        client_config(max),
        Arc::new(SystemClock::new()),
        Box::new(NoJitter),
        Arc::new(NoopSleeper),
        format!("hub:{}", hub.node),
    )
    .unwrap();
    (client, book)
}

/// The response for records 21 to 30 answers the call for 11 to 20: the gap is refused, nothing
/// past it is applied, and a plain pull then converges with each record applied once.
#[tokio::test]
async fn a_response_that_skips_ahead_is_refused_at_the_gap() {
    let rig = pull_rig(30, vec![Action::Pass, Action::Replace(21)]);
    let mut receiver = counting_receiver();

    let first = rig
        .client
        .pull_once(&mut receiver, &rig.source)
        .await
        .unwrap();
    assert!(matches!(first, PullStep::Applied(ack) if ack.through_seq == 10));

    let skipped = rig
        .client
        .pull_once(&mut receiver, &rig.source)
        .await
        .unwrap();
    assert!(
        matches!(&skipped, PullStep::Nacked(Nack::Gap { expected: 11, .. })),
        "the skipping response was not refused as a gap: {skipped:?}; {}",
        log(&rig.book)
    );
    assert_eq!(
        checkpoint(&receiver, &rig.source),
        10,
        "the checkpoint moved"
    );
    assert_eq!(
        receiver.sink().calls,
        (1..=10).collect::<Vec<_>>(),
        "a record past the gap was applied; {}",
        log(&rig.book)
    );

    let third = rig
        .client
        .pull_once(&mut receiver, &rig.source)
        .await
        .unwrap();
    assert!(matches!(third, PullStep::Applied(ack) if ack.through_seq == 20));
    let applied = rig
        .client
        .pull_until_idle(&mut receiver, &rig.source, 20)
        .await
        .unwrap();
    assert_eq!(applied, 30);
    assert_eq!(
        receiver.sink().calls,
        (1..=30).collect::<Vec<_>>(),
        "each record applied exactly once, in order; {}",
        log(&rig.book)
    );
    assert_eq!(receiver.sink().inner.len(), 30);
}

/// The response for 1 to 10 arrives a second time after 11 to 20 was applied: it is acknowledged
/// at the true checkpoint, the checkpoint does not move back and nothing is applied twice.
#[tokio::test]
async fn a_repeated_response_is_acknowledged_and_applied_zero_times() {
    let rig = pull_rig(30, vec![Action::Pass, Action::Pass, Action::Replace(1)]);
    let mut receiver = counting_receiver();
    for through in [10, 20] {
        let step = rig
            .client
            .pull_once(&mut receiver, &rig.source)
            .await
            .unwrap();
        assert!(matches!(step, PullStep::Applied(ack) if ack.through_seq == through));
    }

    let stale = rig
        .client
        .pull_once(&mut receiver, &rig.source)
        .await
        .unwrap();
    assert!(
        matches!(&stale, PullStep::Applied(ack) if ack.through_seq == 20),
        "the stale response was not acknowledged at the true checkpoint: {stale:?}; {}",
        log(&rig.book)
    );
    assert_eq!(
        checkpoint(&receiver, &rig.source),
        20,
        "the checkpoint moved back"
    );
    assert_eq!(
        receiver.sink().calls,
        (1..=20).collect::<Vec<_>>(),
        "the stale batch was applied again; {}",
        log(&rig.book)
    );

    let applied = rig
        .client
        .pull_until_idle(&mut receiver, &rig.source, 20)
        .await
        .unwrap();
    assert_eq!(applied, 30);
    assert_eq!(receiver.sink().calls, (1..=30).collect::<Vec<_>>());
}

/// The first response is held (lost to the caller), the retry is answered, and the held one is
/// released late as the answer to a later call: same result as the repeated response.
#[tokio::test]
async fn a_late_released_response_is_applied_zero_times() {
    // 0: held (the caller retries); 1: the retry, answered; 2: the next pull; 3: the held
    // response, delivered late in place of the answer to the pull for 21 to 30.
    let rig = pull_rig(
        30,
        vec![
            Action::Hold,
            Action::Pass,
            Action::Pass,
            Action::ReleaseHeld(0),
        ],
    );
    let mut receiver = counting_receiver();

    let first = rig
        .client
        .pull_once(&mut receiver, &rig.source)
        .await
        .unwrap();
    assert!(
        matches!(first, PullStep::Applied(ack) if ack.through_seq == 10),
        "the retry after a lost response did not apply 1 to 10; {}",
        log(&rig.book)
    );
    let second = rig
        .client
        .pull_once(&mut receiver, &rig.source)
        .await
        .unwrap();
    assert!(matches!(second, PullStep::Applied(ack) if ack.through_seq == 20));

    let late = rig
        .client
        .pull_once(&mut receiver, &rig.source)
        .await
        .unwrap();
    assert!(
        matches!(&late, PullStep::Applied(ack) if ack.through_seq == 20),
        "the late response was not acknowledged at the true checkpoint: {late:?}; {}",
        log(&rig.book)
    );
    assert_eq!(checkpoint(&receiver, &rig.source), 20);
    assert_eq!(
        receiver.sink().calls,
        (1..=20).collect::<Vec<_>>(),
        "the late response was applied again; {}",
        log(&rig.book)
    );

    let applied = rig
        .client
        .pull_until_idle(&mut receiver, &rig.source, 20)
        .await
        .unwrap();
    assert_eq!(applied, 30);
    assert_eq!(receiver.sink().calls, (1..=30).collect::<Vec<_>>());
}

/// The from_seq of the three responses of a 30-record, 10-per-batch stream.
const SEGMENTS: [u64; 3] = [1, 11, 21];

fn permutations() -> Vec<[usize; 3]> {
    vec![
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ]
}

/// For every order in which the three responses can be delivered, with and without a lost
/// response before each delivery, the node ends at 30 with 30 applications in strictly
/// increasing order.
#[tokio::test]
async fn every_delivery_order_of_three_responses_converges() {
    for lossy in [false, true] {
        for order in permutations() {
            let mut plan = Vec::new();
            for segment in order {
                if lossy {
                    plan.push(Action::Hold);
                }
                plan.push(Action::Replace(SEGMENTS[segment]));
            }
            let rig = pull_rig(30, plan);
            let mut receiver = counting_receiver();
            let applied = rig
                .client
                .pull_until_idle(&mut receiver, &rig.source, 20)
                .await
                .unwrap();
            let context = format!("order {order:?} lossy {lossy}; actions: {}", log(&rig.book));
            assert_eq!(applied, 30, "did not reach 30; {context}");
            assert_eq!(checkpoint(&receiver, &rig.source), 30, "{context}");
            assert_eq!(
                receiver.sink().calls.len(),
                30,
                "not 30 applications in total: {:?}; {context}",
                receiver.sink().calls
            );
            assert!(
                strictly_increasing(&receiver.sink().calls),
                "applied out of order or twice: {:?}; {context}",
                receiver.sink().calls
            );
            assert_eq!(
                receiver.sink().calls,
                (1..=30).collect::<Vec<_>>(),
                "{context}"
            );
        }
    }
}

/// A push rig: a hub that applies pushes, and a client whose transport is the double.
struct Push {
    source: SourceId,
    hub: Hub,
    client: EdgeClient,
    book: Arc<Mutex<Book>>,
}

fn push_rig(plan: Vec<Action>) -> Push {
    let source = SourceId::new("edge-khi#branch_01").unwrap();
    let certificate = certificate(42);
    let node = NodeId::new("edge-khi#branch_01").unwrap();
    let hub = hub(
        Arc::new(directory(&certificate, &node, &["branch_01"])),
        Arc::new(MemoryHub::new()),
        Arc::new(SharedReceiver::new(receiver())),
        settings(),
    );
    let (client, book) = reordering_client(&hub, &node, certificate, &source, 10, plan);
    Push {
        source,
        hub,
        client,
        book,
    }
}

/// The hub's stored checkpoint for the pushed source, and the sequences it applied, in order.
fn hub_state(rig: &Push) -> (u64, Vec<u64>) {
    rig.hub.sink.with(|receiver| {
        let checkpoint = receiver
            .stored_checkpoint(&rig.source)
            .map_or(0, |checkpoint| checkpoint.last_applied_seq);
        let applied = receiver.sink().order().iter().map(|a| a.seq).collect();
        (checkpoint, applied)
    })
}

/// An acknowledgement that arrives late, in place of the answer to the next batch, does not make
/// the sender skip a batch: the hub refuses the batch after the gap, applies nothing past it, and
/// the sender rewinds and sends the missing batch once.
#[tokio::test]
async fn a_late_acknowledgement_never_makes_the_hub_skip_a_batch() {
    // 0: batch 1-10 reaches the hub, its ack is held and the call fails; 1: the retry re-sends
    // 1-10 and is acknowledged (a duplicate at the hub); 2: batch 11-20 is answered with the
    // stale ack for 1-10 and never reaches the hub; 3..: everything is delivered.
    let mut rig = push_rig(vec![Action::Hold, Action::Pass, Action::ReleaseHeld(0)]);
    let offer = Offer::new(rig.source.clone(), records(&rig.source, 1, 30));

    let first = rig.client.push_once(&offer).await.unwrap();
    assert!(
        matches!(first, PushStep::Acked(ref ack) if ack.through_seq == 10),
        "{first:?}; {}",
        log(&rig.book)
    );
    assert_eq!(
        hub_state(&rig),
        (10, (1..=10).collect()),
        "{}",
        log(&rig.book)
    );

    // The stale ack answers the batch for 11 to 20; the hub never saw it.
    let stale = rig.client.push_once(&offer).await.unwrap();
    assert!(
        matches!(stale, PushStep::Acked(ref ack) if ack.through_seq == 10),
        "{stale:?}; {}",
        log(&rig.book)
    );
    assert_eq!(
        rig.client.sender().acked_through(&rig.source),
        10,
        "the sender's acknowledged mark moved on a stale ack; {}",
        log(&rig.book)
    );

    // The sender, believing 11 to 20 is on its way, offers 21 to 30: the hub refuses at the gap.
    let ahead = rig.client.push_once(&offer).await.unwrap();
    assert!(
        matches!(&ahead, PushStep::Nacked(Nack::Gap { expected: 11, .. })),
        "the batch past the gap was not refused: {ahead:?}; {}",
        log(&rig.book)
    );
    assert_eq!(
        hub_state(&rig),
        (10, (1..=10).collect()),
        "the hub applied past the gap; {}",
        log(&rig.book)
    );

    // The sender rewound: the missing batch goes once, then the rest.
    let acked = rig.client.push_until_idle(&offer, 20).await.unwrap();
    assert_eq!(acked, 30, "{}", log(&rig.book));
    assert_eq!(
        hub_state(&rig),
        (30, (1..=30).collect()),
        "each record applied once, in order; {}",
        log(&rig.book)
    );
}

/// An acknowledgement lost to the caller makes the retry re-send the batch: the hub applies it
/// once and the sender never reports more than the hub holds.
#[tokio::test]
async fn a_lost_acknowledgement_is_a_duplicate_batch_applied_once() {
    let mut rig = push_rig(vec![Action::Hold, Action::Pass, Action::Hold, Action::Pass]);
    let offer = Offer::new(rig.source.clone(), records(&rig.source, 1, 30));

    let mut high_water = 0;
    for _ in 0..20 {
        match rig.client.push_once(&offer).await.unwrap() {
            PushStep::Idle => break,
            PushStep::Acked(ack) => {
                let (checkpoint, _) = hub_state(&rig);
                assert!(
                    ack.through_seq <= checkpoint,
                    "an ack through {} is above the hub's checkpoint {checkpoint}; {}",
                    ack.through_seq,
                    log(&rig.book)
                );
                assert!(ack.through_seq >= high_water, "{}", log(&rig.book));
                high_water = ack.through_seq;
            }
            PushStep::Nacked(nack) => panic!("unexpected refusal {nack:?}; {}", log(&rig.book)),
        }
    }
    assert_eq!(high_water, 30, "{}", log(&rig.book));
    assert_eq!(
        hub_state(&rig),
        (30, (1..=30).collect()),
        "applied once each, in order; {}",
        log(&rig.book)
    );
    // Three batches were offered, two of them re-sent after a lost ack: five were forwarded.
    assert_eq!(
        rig.book.lock().unwrap().forwarded_pushes,
        5,
        "{}",
        log(&rig.book)
    );
}

/// A stale acknowledgement for the last batch leaves the sender's cursor ahead of the hub. The
/// acknowledged mark stays honest, the hub holds exactly what it received, and the next record
/// the node offers is refused at the gap, which rewinds the sender so the missing batch is sent.
#[tokio::test]
async fn a_stale_acknowledgement_for_the_last_batch_is_recovered_by_the_next_offer() {
    // 0: 1-10 passes; 1: 11-20 passes; 2: batch 21-30 answered with a stale ack for 1-10 (held
    // below by the first request), so the hub never sees it.
    let mut rig = push_rig(vec![
        Action::Hold,
        Action::Pass,
        Action::Pass,
        Action::ReleaseHeld(0),
    ]);
    let offer = Offer::new(rig.source.clone(), records(&rig.source, 1, 30));
    let acked = rig.client.push_until_idle(&offer, 20).await.unwrap();
    assert_eq!(acked, 20, "{}", log(&rig.book));
    assert_eq!(
        hub_state(&rig),
        (20, (1..=20).collect()),
        "{}",
        log(&rig.book)
    );

    // The node offers more: 31 to 40 is refused at the gap (the hub expects 21), the sender
    // rewinds to 21 and everything arrives once.
    let offer = Offer::new(rig.source.clone(), records(&rig.source, 1, 40));
    let acked = rig.client.push_until_idle(&offer, 20).await.unwrap();
    assert_eq!(acked, 40, "{}", log(&rig.book));
    assert_eq!(
        hub_state(&rig),
        (40, (1..=40).collect()),
        "each record applied once, in order; {}",
        log(&rig.book)
    );
}
