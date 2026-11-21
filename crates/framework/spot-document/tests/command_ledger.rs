//! The per-site command ledger (WP-DOC-04.T8) on the fake backend: a repeated command id is
//! answered with the original outcome and runs no participant, step or write a second time, and
//! retention is configuration, recorded per row.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{n, now, valid_invoice};
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, OffsetDateTime, RecordId, SiteId};
use spot_app_api::{AppResult, BoxFuture, DocState, Document};
use spot_core::ManualClock;
use spot_document::engine::{
    Engine, RetentionPolicy, TxParticipant, V7Ids, WriteIntent, WriteRecord,
};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{CommandRecord, StorageBackend, Transaction};
use spot_storage_fake::{FakeBackend, FakeTx};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Counts every document write the engine makes (a participant runs around each one).
#[derive(Default)]
struct Writes(AtomicUsize);

impl TxParticipant<FakeTx> for Writes {
    fn before_write<'a>(
        &'a self,
        _tx: &'a mut FakeTx,
        _write: &'a WriteIntent<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn after_write<'a>(
        &'a self,
        _tx: &'a mut FakeTx,
        _write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

impl Writes {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

fn policy() -> RetentionPolicy {
    RetentionPolicy::new(
        7 * DAY,
        30 * DAY,
        BTreeMap::from([
            ("bpm".to_owned(), 30 * DAY),
            ("http_api".to_owned(), DAY),
            ("too_long".to_owned(), 90 * DAY),
        ]),
    )
    .unwrap()
}

fn engine_over(
    backend: FakeBackend,
    policy: RetentionPolicy,
    clock: Arc<ManualClock>,
) -> (Engine<FakeBackend>, Arc<Writes>) {
    let engine = Engine::new(
        backend,
        common::meta(),
        Arc::new(RegistryBuilder::new().build().unwrap()),
        clock,
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_command_ledger(policy);
    let writes = Arc::new(Writes::default());
    engine.set_participants(vec![writes.clone() as Arc<dyn TxParticipant<FakeTx>>]);
    (engine, writes)
}

async fn fixture() -> (Engine<FakeBackend>, Arc<Writes>, FakeBackend) {
    let backend = FakeBackend::new();
    let (engine, writes) =
        engine_over(backend.clone(), policy(), Arc::new(ManualClock::new(now())));
    engine.prepare().await.unwrap();
    (engine, writes, backend)
}

fn user() -> Actor {
    Actor::User {
        id: RecordId::new_v7(),
        via: None,
    }
}

fn inv(actor: &Actor, command: Option<&str>) -> Invocation {
    Invocation {
        site: SiteId::new("ledger.test").unwrap(),
        actor: actor.clone(),
        command_id: command.map(|c| CommandId::new(c).unwrap()),
        language: None,
        now: now(),
    }
}

fn customer(name: &str) -> Document {
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc
}

async fn record(backend: &FakeBackend, actor: &Actor, command: &str) -> Option<CommandRecord> {
    let tx = backend
        .begin(
            Arc::new(common::meta().catalog(collation()).unwrap()),
            spot_storage_contract::TxOptions::new(
                n("probe"),
                spot_storage_contract::Isolation::ReadCommitted,
            )
            .read_only(),
        )
        .await
        .unwrap();
    let found = tx
        .command_get(&actor.principal_text(), command)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    found
}

fn collation() -> spot_storage_contract::Collation {
    spot_storage_contract::Collation::new("und").unwrap()
}

#[tokio::test]
async fn create_twice_with_one_command_id_makes_one_document() {
    let (engine, writes, _) = fixture().await;
    let actor = user();
    let doc = customer("Acme");
    let first = engine
        .create(&inv(&actor, Some("cmd-1")), doc.clone())
        .await
        .unwrap();
    assert_eq!(writes.count(), 1);
    let second = engine
        .create(&inv(&actor, Some("cmd-1")), doc)
        .await
        .unwrap();
    assert_eq!(first.value.id(), second.value.id());
    assert_eq!(first.value.version(), second.value.version());
    assert_eq!(first.messages, second.messages);
    // The second answer ran no participant and wrote nothing.
    assert_eq!(writes.count(), 1);
}

#[tokio::test]
async fn a_restarted_engine_over_the_same_store_replays() {
    let (engine, writes, backend) = fixture().await;
    let actor = user();
    let doc = customer("Acme");
    let first = engine
        .create(&inv(&actor, Some("cmd-1")), doc.clone())
        .await
        .unwrap();
    assert_eq!(writes.count(), 1);
    drop(engine);

    let (again, writes_again) = engine_over(backend, policy(), Arc::new(ManualClock::new(now())));
    again.prepare().await.unwrap();
    let second = again
        .create(&inv(&actor, Some("cmd-1")), doc)
        .await
        .unwrap();
    assert_eq!(first.value.id(), second.value.id());
    assert_eq!(writes_again.count(), 0);
}

#[tokio::test]
async fn the_same_id_with_a_different_body_is_a_conflict_and_writes_nothing() {
    let (engine, writes, _) = fixture().await;
    let actor = user();
    engine
        .create(&inv(&actor, Some("cmd-1")), customer("Acme"))
        .await
        .unwrap();
    let other = customer("Globex");
    let error = engine
        .create(&inv(&actor, Some("cmd-1")), other.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "document.command_conflict");
    assert_eq!(writes.count(), 1);
    // The other document does not exist.
    let loaded = engine
        .load(&inv(&actor, None), "customer", other.id())
        .await
        .unwrap_err();
    assert_eq!(loaded.code, "document.not_found");
}

#[tokio::test]
async fn a_failed_first_attempt_is_not_recorded_and_a_corrected_retry_succeeds() {
    let (engine, _, backend) = fixture().await;
    let actor = user();
    // `name` is required: validation refuses and the transaction rolls back.
    let bad = Document::new(n("customer"), RecordId::new_v7());
    engine
        .create(&inv(&actor, Some("cmd-1")), bad)
        .await
        .unwrap_err();
    assert!(record(&backend, &actor, "cmd-1").await.is_none());

    let created = engine
        .create(&inv(&actor, Some("cmd-1")), customer("Acme"))
        .await
        .unwrap();
    assert!(record(&backend, &actor, "cmd-1").await.is_some());
    assert_eq!(created.value.get("name").clone(), "Acme".into());
}

#[tokio::test]
async fn a_rolled_back_transaction_leaves_no_row() {
    let (_, _, backend) = fixture().await;
    let actor = user();
    let catalog = Arc::new(common::meta().catalog(collation()).unwrap());
    let mut tx = backend
        .begin(
            catalog,
            spot_storage_contract::TxOptions::new(
                n("probe"),
                spot_storage_contract::Isolation::ReadCommitted,
            ),
        )
        .await
        .unwrap();
    let rec = CommandRecord::new(
        actor.principal_text(),
        "cmd-1",
        vec![1],
        "{}",
        now(),
        now() + time::Duration::days(1),
    )
    .unwrap();
    tx.command_put(rec.clone()).await.unwrap();
    // Seen by its own transaction, written once.
    assert!(
        tx.command_get(&rec.principal, "cmd-1")
            .await
            .unwrap()
            .is_some()
    );
    assert!(tx.command_put(rec).await.is_err());
    tx.rollback().await.unwrap();
    assert!(record(&backend, &actor, "cmd-1").await.is_none());
}

#[tokio::test]
async fn two_principals_using_one_id_never_see_each_others_record() {
    let (engine, writes, backend) = fixture().await;
    let (alice, bob) = (user(), user());
    let a = engine
        .create(&inv(&alice, Some("cmd-1")), customer("Acme"))
        .await
        .unwrap();
    let b = engine
        .create(&inv(&bob, Some("cmd-1")), customer("Globex"))
        .await
        .unwrap();
    assert_ne!(a.value.id(), b.value.id());
    assert_eq!(writes.count(), 2);
    assert!(record(&backend, &alice, "cmd-1").await.is_some());
    assert!(record(&backend, &bob, "cmd-1").await.is_some());
}

#[tokio::test]
async fn without_a_command_id_the_ledger_is_not_touched() {
    let (engine, writes, _) = fixture().await;
    let actor = user();
    engine
        .create(&inv(&actor, None), customer("Acme"))
        .await
        .unwrap();
    engine
        .create(&inv(&actor, None), customer("Globex"))
        .await
        .unwrap();
    assert_eq!(writes.count(), 2);
    // Nothing was recorded, however far ahead the prune looks.
    let far = now() + time::Duration::days(10_000);
    assert_eq!(engine.prune_commands(far).await.unwrap(), 0);
}

#[tokio::test]
async fn a_replayed_post_after_a_cancel_answers_the_stored_outcome_with_the_current_document() {
    let (engine, writes, _) = fixture().await;
    let actor = user();
    let customer = engine
        .create(&inv(&actor, None), customer("Acme"))
        .await
        .unwrap()
        .value;
    let mut draft = valid_invoice();
    draft.set(n("customer"), customer.id());
    let draft = engine
        .create(&inv(&actor, None), draft)
        .await
        .unwrap()
        .value;
    let posted = engine
        .post(
            &inv(&actor, Some("post-1")),
            "sales_invoice",
            draft.id(),
            draft.version().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(posted.value.state(), Some(DocState::Posted));
    let writes_after_post = writes.count();
    engine
        .cancel(
            &inv(&actor, None),
            "sales_invoice",
            draft.id(),
            posted.value.version().unwrap(),
        )
        .await
        .unwrap();
    let writes_after_cancel = writes.count();

    // The same post again: the stored outcome, with the document as it is now.
    let again = engine
        .post(
            &inv(&actor, Some("post-1")),
            "sales_invoice",
            draft.id(),
            draft.version().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(again.value.id(), draft.id());
    assert_eq!(again.value.state(), Some(DocState::Cancelled));
    assert_eq!(writes.count(), writes_after_cancel);
    assert!(writes_after_post < writes_after_cancel);
}

// ---------------------------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------------------------

#[test]
fn retention_is_resolved_per_class_clamped_and_falls_back_to_the_default() {
    let policy = policy();
    let at = now();
    let days = |d: i64| at + time::Duration::days(d);
    assert_eq!(policy.retain_until(Some("bpm"), at), days(30));
    assert_eq!(policy.retain_until(Some("http_api"), at), days(1));
    // Longer than the cap: clamped, and reported.
    assert_eq!(policy.retain_until(Some("too_long"), at), days(30));
    assert_eq!(policy.clamped_classes(), vec!["too_long"]);
    // Unknown or absent class: the default.
    assert_eq!(policy.retain_until(Some("nobody"), at), days(7));
    assert_eq!(policy.retain_until(None, at), days(7));
}

#[test]
fn an_unusable_policy_is_refused() {
    assert!(RetentionPolicy::new(Duration::ZERO, DAY, BTreeMap::new()).is_err());
    assert!(RetentionPolicy::new(7 * DAY, DAY, BTreeMap::new()).is_err());
    assert!(
        RetentionPolicy::new(
            DAY,
            7 * DAY,
            BTreeMap::from([("c".to_owned(), Duration::ZERO)])
        )
        .is_err()
    );
    let defaults = RetentionPolicy::default();
    assert_eq!(defaults.default_duration(), 7 * DAY);
    assert_eq!(defaults.max_duration(), 30 * DAY);
}

#[tokio::test]
async fn a_row_gets_the_retention_of_its_callers_class_at_write_time() {
    let (engine, _, backend) = fixture().await;
    let actor = user();
    engine
        .for_class("bpm")
        .create(&inv(&actor, Some("a")), customer("Acme"))
        .await
        .unwrap();
    engine
        .for_class("http_api")
        .create(&inv(&actor, Some("b")), customer("Globex"))
        .await
        .unwrap();
    engine
        .create(&inv(&actor, Some("c")), customer("Initech"))
        .await
        .unwrap();
    let a = record(&backend, &actor, "a").await.unwrap();
    let b = record(&backend, &actor, "b").await.unwrap();
    let c = record(&backend, &actor, "c").await.unwrap();
    assert_eq!(a.recorded_at, b.recorded_at);
    assert_eq!(a.retain_until - a.recorded_at, time::Duration::days(30));
    assert_eq!(b.retain_until - b.recorded_at, time::Duration::days(1));
    assert_eq!(c.retain_until - c.recorded_at, time::Duration::days(7));
}

#[tokio::test]
async fn changing_the_policy_afterwards_does_not_alter_existing_rows() {
    let backend = FakeBackend::new();
    let clock = Arc::new(ManualClock::new(now()));
    let (engine, _) = engine_over(backend.clone(), policy(), clock.clone());
    engine.prepare().await.unwrap();
    let actor = user();
    let doc = customer("Acme");
    engine
        .create(&inv(&actor, Some("old")), doc.clone())
        .await
        .unwrap();
    let before = record(&backend, &actor, "old").await.unwrap();
    assert_eq!(
        before.retain_until - before.recorded_at,
        time::Duration::days(7)
    );
    drop(engine);

    let one_day = RetentionPolicy::new(DAY, DAY, BTreeMap::new()).unwrap();
    let (rebuilt, writes) = engine_over(backend.clone(), one_day, clock);
    rebuilt.prepare().await.unwrap();
    // The old row keeps its date and still replays.
    let after = record(&backend, &actor, "old").await.unwrap();
    assert_eq!(after.retain_until, before.retain_until);
    rebuilt
        .create(&inv(&actor, Some("old")), doc)
        .await
        .unwrap();
    assert_eq!(writes.count(), 0);
    // A new row follows the new policy.
    rebuilt
        .create(&inv(&actor, Some("new")), customer("Globex"))
        .await
        .unwrap();
    let new = record(&backend, &actor, "new").await.unwrap();
    assert_eq!(new.retain_until - new.recorded_at, time::Duration::days(1));
}

#[tokio::test]
async fn prune_deletes_by_retain_until_only_and_the_boundary_is_inclusive() {
    let (engine, _, backend) = fixture().await;
    let actor = user();
    let catalog = Arc::new(common::meta().catalog(collation()).unwrap());
    let long_ago = now() - time::Duration::days(400);
    let put = |command: &'static str, recorded: OffsetDateTime, retain: OffsetDateTime| {
        let backend = backend.clone();
        let catalog = catalog.clone();
        let principal = actor.principal_text();
        async move {
            let mut tx = backend
                .begin(
                    catalog,
                    spot_storage_contract::TxOptions::new(
                        n("probe"),
                        spot_storage_contract::Isolation::ReadCommitted,
                    ),
                )
                .await
                .unwrap();
            let rec =
                CommandRecord::new(principal, command, vec![1], "{}", recorded, retain).unwrap();
            tx.command_put(rec).await.unwrap();
            tx.commit().await.unwrap();
        }
    };
    let cutoff = now() + time::Duration::days(1);
    // Recorded long ago, but its own date is after the cut-off: survives.
    put("kept", long_ago, cutoff + time::Duration::days(1)).await;
    // Recorded just now, with a date before the cut-off: deleted.
    put("gone", now(), cutoff - time::Duration::hours(1)).await;
    // Retention ends exactly at the cut-off: deleted (`retain_until <= before`).
    put("edge", long_ago, cutoff).await;

    assert_eq!(engine.prune_commands(cutoff).await.unwrap(), 2);
    assert!(record(&backend, &actor, "kept").await.is_some());
    assert!(record(&backend, &actor, "gone").await.is_none());
    assert!(record(&backend, &actor, "edge").await.is_none());
    // A pruned row is gone: a second prune finds nothing more.
    assert_eq!(engine.prune_commands(cutoff).await.unwrap(), 0);
}
