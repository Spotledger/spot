//! A stale version met by a step's nested write is not the caller's 412 (WP-DOC-04.T7).
//!
//! A posted DocType `entry` links a shared `parent`. Its `after_post` step reads the parent with
//! the repository's plain `load`, adds the entry's hours to the parent's counter and writes it
//! under the version it loaded: the shape of the sample app's roll-up. Concurrent posts onto one
//! parent collide in that nested write. The engine must rerun the whole operation, so every post
//! succeeds and no caller is told `document.stale_version` for a document it never named.
//!
//! Runs on PostgreSQL: the collision needs real row locks and `READ COMMITTED`. Needs
//! `SPOT_TEST_PG_URL`; skips (printing why) without it, and fails under `SPOT_REQUIRE_PG=1`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{n, now, with_default};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, Document, DocumentPatch, ErrorClass, HookPoint,
    Phase, Target,
};
use spot_core::ManualClock;
use spot_document::engine::{Engine, RetryPolicy, V7Ids};
use spot_events_contract::lifecycle::Operation;
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_seams::RegistryBuilder;
use spot_storage_contract::ErrorKind;

/// Concurrent posts onto one parent.
const POSTS: usize = 16;
/// Rounds of the main test, each with fresh drafts.
const ROUNDS: usize = 50;

/// What the `after_post` step does after its roll-up write.
#[derive(Clone, Copy)]
enum Mode {
    /// Propagate a failed write (the sample app's behaviour).
    Propagate,
    /// Catch a failed write and carry on without the roll-up.
    Catch,
    /// Catch a failed write, then refuse with an unrelated error of its own.
    CatchThenRefuse,
}

/// The roll-up of `entry`: `parent.total += entry.hours`, under the version it loaded.
struct Rollup(Mode);

impl Controller for Rollup {
    fn doctype(&self) -> &str {
        "entry"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("rollup:after_post").unwrap(),
            point: HookPoint::of(Phase::After, Target::Op(Operation::Post)),
            priority: 0,
            budget: Duration::from_secs(5),
        }]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let hours = ctx.doc().int("hours")?.unwrap_or(0);
            let parent_id = ctx.doc().link("owner")?.expect("a linked parent");
            let parent = ctx.load("parent", parent_id).await?;
            let total = parent.int("total")?.unwrap_or(0);
            let mut patch = DocumentPatch::default();
            patch.fields.insert(n("total"), Value::Int(total + hours));
            let written = ctx
                .update("parent", parent_id, patch, parent.version().unwrap())
                .await;
            match (written, self.0) {
                (Ok(_), _) => Ok(()),
                (Err(e), Mode::Propagate) => Err(e),
                (Err(_), Mode::Catch) => Ok(()),
                (Err(_), Mode::CatchThenRefuse) => Err(AppError::new(
                    ErrorClass::InvalidState,
                    "document.invalid_state",
                    "The step refuses after its own handling.",
                )),
            }
        })
    }
}

fn meta() -> Arc<EffectiveMeta> {
    let mut parent = Definition::new("parent");
    parent.fields = vec![with_default(FieldSpec::new("total", FieldType::Int), "0")];
    let mut entry = Definition::new("entry");
    entry.is_posted = true;
    let mut link = FieldSpec::new("owner", FieldType::Link);
    link.options = Some("parent".into());
    link.required = true;
    entry.fields = vec![link, FieldSpec::new("hours", FieldType::Int)];
    Registry::new(vec![parent, entry]).unwrap().effective()
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("nested.conflict.test").unwrap(),
        actor: Actor::System {
            component: n("nested_conflict_tests"),
        },
        command_id: CommandId::new("cmd-nested").ok(),
        language: None,
        now: now(),
    }
}

async fn engine(db: &TestDb, mode: Mode, retry: RetryPolicy) -> Arc<Engine<PgBackend>> {
    let backend = if std::env::var_os("SPOT_PG_UNCHECKED").is_some() {
        PgBackend::connect_unchecked(db.config()).unwrap()
    } else {
        PgBackend::connect(db.config()).await.unwrap()
    };
    let mut builder = RegistryBuilder::new();
    builder.add_controller("rollup", Arc::new(Rollup(mode)));
    let engine = Engine::new(
        backend,
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_retry_policy(retry);
    engine.prepare().await.unwrap();
    Arc::new(engine)
}

/// A retry policy with room for every post of a round to lose to the others, and no waiting.
fn roomy() -> RetryPolicy {
    RetryPolicy {
        max_retries: 64,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    }
}

async fn new_parent(engine: &Engine<PgBackend>) -> Document {
    let doc = Document::new(n("parent"), RecordId::new_v7());
    engine.create(&inv(), doc).await.unwrap().value
}

async fn new_entry(engine: &Engine<PgBackend>, parent: RecordId, hours: i64) -> Document {
    let mut doc = Document::new(n("entry"), RecordId::new_v7());
    doc.set(n("owner"), Value::Id(parent));
    doc.set(n("hours"), Value::Int(hours));
    engine.create(&inv(), doc).await.unwrap().value
}

async fn parent_state(engine: &Engine<PgBackend>, id: RecordId) -> (i64, Version) {
    let doc = engine.peek("parent", id).await.unwrap().unwrap();
    (doc.int("total").unwrap().unwrap(), doc.version().unwrap())
}

/// Posts one draft per task, all at once, and returns every outcome.
async fn post_all(
    engine: &Arc<Engine<PgBackend>>,
    drafts: Vec<Document>,
) -> Vec<AppResult<Document>> {
    let tasks: Vec<_> = drafts
        .into_iter()
        .map(|draft| {
            let engine = Arc::clone(engine);
            tokio::spawn(async move {
                engine
                    .post(&inv(), "entry", draft.id(), draft.version().unwrap())
                    .await
                    .map(|o| o.value)
            })
        })
        .collect();
    let mut out = Vec::new();
    for task in tasks {
        out.push(task.await.unwrap());
    }
    out
}

macro_rules! scratch {
    () => {
        match TestDb::create().await {
            Some(db) => db,
            None => {
                assert!(
                    std::env::var_os("SPOT_REQUIRE_PG").is_none(),
                    "SPOT_REQUIRE_PG is set but SPOT_TEST_PG_URL gives no database"
                );
                return;
            }
        }
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_posts_onto_one_parent_all_succeed() {
    let db = scratch!();
    let engine = engine(&db, Mode::Propagate, roomy()).await;
    let parent = new_parent(&engine).await;
    let mut expected_total = 0i64;
    let mut expected_version = parent.version().unwrap().0;
    for round in 0..ROUNDS {
        let mut drafts = Vec::new();
        for k in 0..POSTS {
            let hours = i64::try_from(round * POSTS + k + 1).unwrap();
            expected_total += hours;
            drafts.push(new_entry(&engine, parent.id(), hours).await);
        }
        expected_version += u64::try_from(POSTS).unwrap();
        for (k, outcome) in post_all(&engine, drafts).await.into_iter().enumerate() {
            let error = outcome.err();
            assert!(
                error.is_none(),
                "round {round}, post {k} failed: {error:?} (a nested stale must rerun, not surface)"
            );
        }
        let (total, version) = parent_state(&engine, parent.id()).await;
        assert_eq!(
            total, expected_total,
            "round {round}: every roll-up landed once"
        );
        assert_eq!(
            version.0, expected_version,
            "round {round}: one version per post"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn with_the_default_policy_a_post_succeeds_or_fails_as_a_conflict() {
    let db = scratch!();
    let engine = engine(&db, Mode::Propagate, RetryPolicy::default()).await;
    let parent = new_parent(&engine).await;
    let mut drafts = Vec::new();
    for _ in 0..POSTS {
        drafts.push(new_entry(&engine, parent.id(), 1).await);
    }
    let ids: Vec<RecordId> = drafts.iter().map(Document::id).collect();
    let outcomes = post_all(&engine, drafts).await;
    let mut succeeded = 0u64;
    for (id, outcome) in ids.iter().zip(outcomes) {
        match outcome {
            Ok(_) => succeeded += 1,
            Err(e) => {
                assert_eq!(e.code, "document.conflict", "{e:?}");
                assert_ne!(e.code, "document.stale_version");
                assert_eq!(e.class, ErrorClass::Unavailable);
                assert!(e.retryable());
                // It names the document that conflicted, and carries no version of it.
                assert_eq!(
                    e.details.get("doctype").and_then(|v| v.as_str()),
                    Some("parent")
                );
                assert_eq!(
                    e.details.get("id").and_then(|v| v.as_str()),
                    Some(parent.id().to_string().as_str())
                );
                assert!(e.details.get("current_version").is_none(), "{e:?}");
                // A failed post left its draft a draft.
                let draft = engine.peek("entry", *id).await.unwrap().unwrap();
                assert_eq!(draft.version(), Some(Version(1)), "the draft is untouched");
            }
        }
    }
    eprintln!("{succeeded} of {POSTS} posts succeeded under the default policy");
    assert!(succeeded >= 1, "at least one post wins every round");
    let (total, version) = parent_state(&engine, parent.id()).await;
    assert_eq!(
        total,
        i64::try_from(succeeded).unwrap(),
        "only committed posts rolled up"
    );
    assert_eq!(version.0, 1 + succeeded, "one version per committed post");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_entrys_own_stale_version_stays_document_stale_version() {
    let db = scratch!();
    let engine = engine(&db, Mode::Propagate, RetryPolicy::default()).await;
    let parent = new_parent(&engine).await;
    let draft = new_entry(&engine, parent.id(), 3).await;
    let wrong = Version(draft.version().unwrap().0 + 5);
    let error = engine
        .post(&inv(), "entry", draft.id(), wrong)
        .await
        .unwrap_err();
    assert_eq!(error.code, "document.stale_version");
    assert_eq!(error.class, ErrorClass::StaleVersion);
    assert_eq!(
        error
            .details
            .get("current_version")
            .and_then(|v| v.as_u64()),
        Some(draft.version().unwrap().0)
    );
    assert!(engine.retries().is_empty(), "an own stale is never rerun");
    assert_eq!(
        parent_state(&engine, parent.id()).await.0,
        0,
        "no roll-up ran"
    );
}

/// 16 posts at once onto a fresh parent: they collide in the roll-up write.
async fn collide(engine: &Arc<Engine<PgBackend>>) -> Vec<AppResult<Document>> {
    let parent = new_parent(engine).await;
    let mut drafts = Vec::new();
    for _ in 0..POSTS {
        drafts.push(new_entry(engine, parent.id(), 1).await);
    }
    post_all(engine, drafts).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_step_that_catches_its_nested_stale_commits_without_a_rerun() {
    let db = scratch!();
    let engine = engine(&db, Mode::Catch, RetryPolicy::default()).await;
    for outcome in collide(&engine).await {
        let error = outcome.err();
        assert!(
            error.is_none(),
            "a caught nested stale commits the post: {error:?}"
        );
    }
    let rerun: Vec<_> = engine
        .retries()
        .into_iter()
        .filter(|r| r.cause == ErrorKind::StaleVersion)
        .collect();
    assert!(
        rerun.is_empty(),
        "nothing was rerun for a caught stale: {rerun:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_caught_stale_does_not_make_a_later_refusal_retryable() {
    let db = scratch!();
    let engine = engine(&db, Mode::CatchThenRefuse, RetryPolicy::default()).await;
    let mut refused = 0;
    for outcome in collide(&engine).await {
        match outcome {
            Ok(_) => {}
            Err(e) => {
                assert_eq!(e.code, "document.invalid_state", "{e:?}");
                refused += 1;
            }
        }
    }
    // Only a post that met the parent's newer version reaches the refusal; the others fail
    // earlier... in this controller every post refuses only after a stale it caught, so the
    // posts that did not collide succeed.
    assert!(refused >= 1, "at least one post collided and refused");
    let rerun: Vec<_> = engine
        .retries()
        .into_iter()
        .filter(|r| r.cause == ErrorKind::StaleVersion)
        .collect();
    assert!(
        rerun.is_empty(),
        "a refusal after a caught stale is not rerun: {rerun:?}"
    );
}
