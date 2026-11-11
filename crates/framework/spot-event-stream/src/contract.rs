//! The backend-independent [`ConsumerStore`] contract suite (feature `testing`).
//!
//! Every store backend must pass [`run_all`]: the in-memory one here
//! (`tests/store_contract.rs`) and a database backend (a later work package). The suite reads
//! time only through the values it passes in and never sleeps.
//!
//! ```ignore
//! let report = run_all(|| async { Arc::new(InMemoryConsumerStore::new()) as Arc<dyn ConsumerStore> }).await;
//! report.assert_all_passed();
//! ```

use std::future::Future;
use std::sync::Arc;

use spot_events_contract::envelope::Position;
use time::OffsetDateTime;

use crate::error::StreamError;
use crate::spec::{ConsumerSpec, PayloadMode, Start};
use crate::store::{ConsumerRecord, ConsumerStore, RegisterOutcome};

macro_rules! ensure {
    ($cond:expr, $($why:tt)+) => {
        if !$cond {
            return Err(format!($($why)+));
        }
    };
}

/// How one case ended: `Err` carries what was wrong.
pub type CheckResult = Result<(), String>;

/// One case's name and outcome.
#[derive(Debug)]
pub struct CaseResult {
    /// The case's name.
    pub name: &'static str,
    /// `Ok` when the backend behaved.
    pub outcome: CheckResult,
}

/// The outcome of [`run_all`].
#[derive(Debug, Default)]
pub struct Report {
    /// One entry per case, in run order.
    pub cases: Vec<CaseResult>,
}

impl Report {
    /// The cases that failed.
    #[must_use]
    pub fn failures(&self) -> Vec<&CaseResult> {
        self.cases.iter().filter(|c| c.outcome.is_err()).collect()
    }

    /// Panics, listing every failed case, unless all passed (and at least one ran).
    ///
    /// # Panics
    ///
    /// When a case failed or none ran.
    pub fn assert_all_passed(&self) {
        assert!(!self.cases.is_empty(), "the contract suite ran no cases");
        let failures = self.failures();
        if failures.is_empty() {
            return;
        }
        let mut text = format!(
            "{} of {} contract cases failed:",
            failures.len(),
            self.cases.len()
        );
        for case in failures {
            if let Err(why) = &case.outcome {
                text.push_str(&format!("\n  - {}: {why}", case.name));
            }
        }
        panic!("{text}");
    }
}

/// Runs every case, each on a fresh store from `make`.
pub async fn run_all<F, Fut>(make: F) -> Report
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn ConsumerStore>>,
{
    let mut report = Report::default();
    macro_rules! case {
        ($case:path) => {{
            let outcome = $case(make().await).await;
            report.cases.push(CaseResult {
                name: stringify!($case),
                outcome,
            });
        }};
    }
    case!(registration_is_idempotent_and_isolated);
    case!(checkpoints_start_at_the_initial_position);
    case!(advance_moves_forward_only);
    case!(advance_is_a_compare_and_set);
    case!(advance_to_the_same_position_is_idempotent);
    case!(restart_may_move_backwards);
    case!(parking_records_the_instant);
    case!(unknown_consumers_are_refused);
    case!(partitions_are_isolated);
    case!(register_if_absent_creates_once);
    case!(register_if_absent_leaves_an_existing_consumer_untouched);
    case!(concurrent_register_if_absent_creates_exactly_once);
    case!(progress_made_after_a_read_is_never_reset);
    report
}

fn spec(name: &str, partitions: u32) -> ConsumerSpec {
    ConsumerSpec {
        name: name.to_owned(),
        owner: "owner".to_owned(),
        topics: vec![crate::spec::TopicSelector::Prefix("doc".to_owned())],
        filter: None,
        partitions,
        payload_mode: PayloadMode::Fields,
        start: Start::FromBeginning,
    }
}

fn position(generation: u64, seq: u64) -> Position {
    Position { generation, seq }
}

async fn registration_is_idempotent_and_isolated(store: Arc<dyn ConsumerStore>) -> CheckResult {
    let record = ConsumerRecord::new(spec("alpha", 2), OffsetDateTime::UNIX_EPOCH);
    ok(store.register(&record).await, "register")?;
    ok(store.register(&record).await, "re-register")?;
    let listed = ok(store.list().await, "list")?;
    ensure!(listed.len() == 1, "list has {} consumers", listed.len());
    ensure!(
        ok(store.get("alpha").await, "get")?.is_some(),
        "the consumer is not there",
    );
    ok(store.delete("alpha").await, "delete")?;
    ensure!(
        ok(store.get("alpha").await, "get after delete")?.is_none(),
        "delete left the consumer",
    );
    Ok(())
}

async fn checkpoints_start_at_the_initial_position(store: Arc<dyn ConsumerStore>) -> CheckResult {
    let mut record = ConsumerRecord::new(spec("beta", 3), OffsetDateTime::UNIX_EPOCH);
    record.spec.start = Start::FromPosition(position(4, 9));
    record.checkpoints = record.spec.initial_positions();
    ok(store.register(&record).await, "register")?;
    for partition in 0..3 {
        let cp = ok(store.checkpoint("beta", partition).await, "checkpoint")?;
        ensure!(
            cp == position(4, 9),
            "partition {partition} starts at {cp:?}, not the stated position",
        );
    }
    Ok(())
}

async fn advance_moves_forward_only(store: Arc<dyn ConsumerStore>) -> CheckResult {
    let record = ConsumerRecord::new(spec("gamma", 1), OffsetDateTime::UNIX_EPOCH);
    ok(store.register(&record).await, "register")?;
    ok(
        store
            .advance("gamma", 0, position(1, 5), position(0, 0))
            .await,
        "advance forward",
    )?;
    refused(
        store
            .advance("gamma", 0, position(1, 2), position(1, 5))
            .await,
        crate::error::CODE_ACK_REFUSED,
        "advance backwards",
    )?;
    let cp = ok(store.checkpoint("gamma", 0).await, "checkpoint")?;
    ensure!(
        cp == position(1, 5),
        "a refused advance moved the checkpoint"
    );
    Ok(())
}

async fn advance_is_a_compare_and_set(store: Arc<dyn ConsumerStore>) -> CheckResult {
    let record = ConsumerRecord::new(spec("delta", 1), OffsetDateTime::UNIX_EPOCH);
    ok(store.register(&record).await, "register")?;
    ok(
        store
            .advance("delta", 0, position(1, 1), position(0, 0))
            .await,
        "first advance",
    )?;
    // A stale writer: it believes the checkpoint is still the start.
    refused(
        store
            .advance("delta", 0, position(1, 2), position(0, 0))
            .await,
        crate::error::CODE_ACK_REFUSED,
        "stale advance",
    )?;
    let cp = ok(store.checkpoint("delta", 0).await, "checkpoint")?;
    ensure!(cp == position(1, 1), "a stale advance moved the checkpoint");
    Ok(())
}

async fn advance_to_the_same_position_is_idempotent(store: Arc<dyn ConsumerStore>) -> CheckResult {
    let record = ConsumerRecord::new(spec("epsilon", 1), OffsetDateTime::UNIX_EPOCH);
    ok(store.register(&record).await, "register")?;
    ok(
        store
            .advance("epsilon", 0, position(1, 3), position(0, 0))
            .await,
        "first advance",
    )?;
    ok(
        store
            .advance("epsilon", 0, position(1, 3), position(1, 3))
            .await,
        "idempotent advance",
    )?;
    Ok(())
}

async fn restart_may_move_backwards(store: Arc<dyn ConsumerStore>) -> CheckResult {
    let record = ConsumerRecord::new(spec("zeta", 1), OffsetDateTime::UNIX_EPOCH);
    ok(store.register(&record).await, "register")?;
    ok(
        store
            .advance("zeta", 0, position(2, 8), position(0, 0))
            .await,
        "advance",
    )?;
    ok(
        store.restart("zeta", 0, position(1, 1)).await,
        "restart backwards",
    )?;
    let cp = ok(store.checkpoint("zeta", 0).await, "checkpoint")?;
    ensure!(cp == position(1, 1), "restart did not move the checkpoint");
    Ok(())
}

async fn parking_records_the_instant(store: Arc<dyn ConsumerStore>) -> CheckResult {
    let record = ConsumerRecord::new(spec("eta", 1), OffsetDateTime::UNIX_EPOCH);
    ok(store.register(&record).await, "register")?;
    let at = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1);
    ok(store.park("eta", 0, at).await, "park")?;
    let loaded = ok(store.get("eta").await, "get")?.ok_or("the consumer vanished")?;
    ensure!(loaded.parked[0], "the partition is not parked");
    ensure!(
        loaded.parked_since[0] == Some(at),
        "parked_since is {:?}",
        loaded.parked_since[0],
    );
    ok(store.unpark("eta", 0).await, "unpark")?;
    let loaded = ok(store.get("eta").await, "get")?.ok_or("the consumer vanished")?;
    ensure!(!loaded.parked[0], "the partition is still parked");
    Ok(())
}

async fn unknown_consumers_are_refused(store: Arc<dyn ConsumerStore>) -> CheckResult {
    refused(
        store.checkpoint("nope", 0).await,
        crate::error::CODE_CONSUMER_NOT_FOUND,
        "checkpoint of an unknown consumer",
    )?;
    refused(
        store
            .advance("nope", 0, position(1, 1), position(0, 0))
            .await,
        crate::error::CODE_CONSUMER_NOT_FOUND,
        "advance of an unknown consumer",
    )?;
    refused(
        store.park("nope", 0, OffsetDateTime::UNIX_EPOCH).await,
        crate::error::CODE_CONSUMER_NOT_FOUND,
        "park of an unknown consumer",
    )?;
    Ok(())
}

async fn partitions_are_isolated(store: Arc<dyn ConsumerStore>) -> CheckResult {
    let record = ConsumerRecord::new(spec("theta", 2), OffsetDateTime::UNIX_EPOCH);
    ok(store.register(&record).await, "register")?;
    ok(
        store
            .advance("theta", 0, position(1, 4), position(0, 0))
            .await,
        "advance partition 0",
    )?;
    let other = ok(store.checkpoint("theta", 1).await, "checkpoint 1")?;
    ensure!(
        other == position(0, 0),
        "partition 1 moved when partition 0 advanced",
    );
    refused(
        store
            .advance("theta", 5, position(1, 1), position(0, 0))
            .await,
        crate::error::CODE_SPEC_INVALID,
        "a partition out of range",
    )?;
    Ok(())
}

async fn register_if_absent_creates_once(store: Arc<dyn ConsumerStore>) -> CheckResult {
    let record = ConsumerRecord::new(spec("iota", 2), OffsetDateTime::UNIX_EPOCH);
    let first = ok(store.register_if_absent(&record).await, "first")?;
    ensure!(first == RegisterOutcome::Created, "first call: {first:?}");
    let second = ok(store.register_if_absent(&record).await, "second")?;
    ensure!(
        second == RegisterOutcome::Existing(Box::new(record.clone())),
        "second call: {second:?}",
    );
    let listed = ok(store.list().await, "list")?;
    ensure!(listed.len() == 1, "list has {} consumers", listed.len());
    Ok(())
}

async fn register_if_absent_leaves_an_existing_consumer_untouched(
    store: Arc<dyn ConsumerStore>,
) -> CheckResult {
    let record = ConsumerRecord::new(spec("kappa", 2), OffsetDateTime::UNIX_EPOCH);
    ok(store.register_if_absent(&record).await, "create")?;
    ok(
        store
            .advance("kappa", 1, position(2, 6), position(0, 0))
            .await,
        "advance",
    )?;
    let at = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(2);
    ok(store.park("kappa", 0, at).await, "park")?;
    let before = ok(store.get("kappa").await, "get")?.ok_or("the consumer vanished")?;
    // A different spec and start state must not replace anything.
    let mut other = ConsumerRecord::new(spec("kappa", 5), OffsetDateTime::UNIX_EPOCH);
    other.spec.owner = "someone-else".to_owned();
    let outcome = ok(store.register_if_absent(&other).await, "second")?;
    ensure!(
        outcome == RegisterOutcome::Existing(Box::new(before.clone())),
        "the stored record was not returned: {outcome:?}",
    );
    let after = ok(store.get("kappa").await, "get after")?.ok_or("the consumer vanished")?;
    ensure!(
        after == before,
        "register_if_absent changed the stored consumer"
    );
    ensure!(
        after.checkpoints[1] == position(2, 6) && after.parked[0] && after.spec.partitions == 2,
        "checkpoints, parking or spec changed",
    );
    Ok(())
}

async fn concurrent_register_if_absent_creates_exactly_once(
    store: Arc<dyn ConsumerStore>,
) -> CheckResult {
    let record = ConsumerRecord::new(spec("lambda", 3), OffsetDateTime::UNIX_EPOCH);
    let calls: Vec<_> = (0..8).map(|_| store.register_if_absent(&record)).collect();
    let mut created = 0;
    for outcome in join_all(calls).await {
        match ok(outcome, "concurrent register_if_absent")? {
            RegisterOutcome::Created => created += 1,
            RegisterOutcome::Existing(existing) => {
                ensure!(*existing == record, "a loser saw another record");
            }
        }
    }
    ensure!(created == 1, "{created} callers created the consumer");
    Ok(())
}

async fn progress_made_after_a_read_is_never_reset(store: Arc<dyn ConsumerStore>) -> CheckResult {
    // Node A reads nothing; node B registers and acknowledges; node A then registers with its
    // fresh (start) record. A's registration must adopt B's progress, never reset it.
    ensure!(
        ok(store.get("mu").await, "get")?.is_none(),
        "the consumer exists already",
    );
    let fresh = ConsumerRecord::new(spec("mu", 1), OffsetDateTime::UNIX_EPOCH);
    ok(store.register_if_absent(&fresh).await, "node B registers")?;
    ok(
        store.advance("mu", 0, position(1, 9), position(0, 0)).await,
        "node B acknowledges",
    )?;
    let outcome = ok(store.register_if_absent(&fresh).await, "node A registers")?;
    match outcome {
        RegisterOutcome::Existing(existing) => ensure!(
            existing.checkpoints[0] == position(1, 9),
            "node A was handed {:?}",
            existing.checkpoints[0],
        ),
        RegisterOutcome::Created => return Err("node A created over a registered consumer".into()),
    }
    let cp = ok(store.checkpoint("mu", 0).await, "checkpoint")?;
    ensure!(cp == position(1, 9), "the acknowledged progress was reset");
    Ok(())
}

/// Polls every future to completion, all of them interleaved on the calling task.
async fn join_all<T, F: Future<Output = T>>(futures: Vec<F>) -> Vec<T> {
    let mut slots: Vec<(std::pin::Pin<Box<F>>, Option<T>)> =
        futures.into_iter().map(|f| (Box::pin(f), None)).collect();
    std::future::poll_fn(move |cx| {
        let mut pending = false;
        for (future, slot) in &mut slots {
            if slot.is_none() {
                match future.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => *slot = Some(value),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(
                slots
                    .iter_mut()
                    .filter_map(|(_, slot)| slot.take())
                    .collect(),
            )
        }
    })
    .await
}

fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, what: &str) -> Result<T, String> {
    result.map_err(|error| format!("{what} failed: {error:?}"))
}

fn refused<T: std::fmt::Debug>(
    result: Result<T, StreamError>,
    code: &str,
    what: &str,
) -> Result<(), String> {
    match result {
        Err(error) if error.code() == code => Ok(()),
        Err(error) => Err(format!("{what}: expected {code}, got {}", error.code())),
        Ok(value) => Err(format!(
            "{what}: expected {code}, but it succeeded: {value:?}"
        )),
    }
}
