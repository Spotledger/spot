//! The PostgreSQL appender state: one async contract suite run against the in-memory reference and
//! the PostgreSQL store, then the defect reproduction (a real `LaneAppender` over `PgAuditStore`
//! and `PgAppenderStore` draining inside a current-thread and a multi-thread runtime, the case the
//! blocking bridge refused), restart, site isolation, idempotence, an unavailable database, no
//! value in an error, and the migrator's plan.
//!
//! Every test that needs a server skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`
//! and fails when `SPOT_REQUIRE_PG=1`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use spot_audit_chain::{AuditStore, CheckpointSigner, InMemoryStore, LocalSigner, genesis_hash};
use spot_audit_store::{
    APPENDER_TABLE_DDL, APPENDER_TABLES_MISSING, AppenderTables, BOOKMARK_TABLE, EVENT_CONFLICT,
    EVENT_INDEX_TABLE, PgAppenderStore, PgAuditStore, TABLE_DDL, verify_appender,
};
use spot_doc_audit::{
    APPENDER_STATE, ActorRef, AppendResult, AppenderStore, AuditSource, DocAuditError,
    DocAuditEvent, DocOperation, InMemoryAppenderStore, LaneAppender, LaneConfig, LaneHead,
    OutboxFeed, SiteCheckpoint, lane_site, outbox_event_of,
};
use spot_pg_migrate::{ApplyOptions, ExtraTable, PlanOptions, apply, dry_run};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{Catalog, Collation, OutboxRecord, RecordId};

struct Fixture {
    db: TestDb,
}

impl Fixture {
    /// A scratch database with the appender tables created from the public DDL.
    async fn with_tables() -> Option<Fixture> {
        let fx = Fixture::bare().await?;
        fx.exec(
            &APPENDER_TABLE_DDL
                .iter()
                .map(|(_, ddl)| *ddl)
                .collect::<Vec<_>>()
                .join(";"),
        )
        .await;
        Some(fx)
    }

    /// A scratch database with nothing in it.
    async fn bare() -> Option<Fixture> {
        let db = TestDb::create().await?;
        Some(Fixture { db })
    }

    async fn exec(&self, sql: &str) {
        self.db
            .raw_client()
            .await
            .batch_execute(sql)
            .await
            .expect("sql");
    }

    async fn count(&self, table: &str) -> i64 {
        self.db
            .raw_client()
            .await
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await
            .expect("count")
            .get(0)
    }

    fn pool(&self) -> Pool {
        common::pool(&self.db, 8)
    }

    /// A store on a pool of its own, so two stores are two processes.
    fn store(&self, site: &str) -> PgAppenderStore {
        PgAppenderStore::new(self.pool(), site)
    }

    /// The chain store with its tables prepared.
    async fn chain(&self) -> PgAuditStore {
        common::store(&self.db).await
    }
}

/// A pool whose database is not there: nothing listens on the port.
fn nowhere_pool() -> Pool {
    let mut driver = tokio_postgres::Config::new();
    driver
        .host("127.0.0.1")
        .port(1)
        .dbname("nothing")
        .user("nobody")
        .password("nobody")
        .connect_timeout(Duration::from_millis(300));
    let manager = Manager::from_config(
        driver,
        tokio_postgres::NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(2)
        .runtime(Runtime::Tokio1)
        .build()
        .expect("the pool builds")
}

fn signer() -> Arc<dyn CheckpointSigner> {
    Arc::new(LocalSigner::from_seed([7u8; 32]))
}

fn checkpoint(site: &str, at: i64) -> SiteCheckpoint {
    let config = LaneConfig::new(2, 1);
    let heads: Vec<LaneHead> = (0..2)
        .map(|lane| LaneHead {
            lane,
            seq: u64::try_from(at).unwrap_or(0),
            head_hash: genesis_hash(&lane_site(site, lane)),
        })
        .collect();
    SiteCheckpoint::sign(&*signer(), site, &config, &heads, at).expect("checkpoint")
}

/// Everything `InMemoryAppenderStore` does, asserted on any [`AppenderStore`]. One suite for both
/// stores, so they cannot drift.
async fn contract(store: &dyn AppenderStore, site: &str) {
    // A new store: no bookmark, nothing chained, no document history, no checkpoint.
    assert_eq!(store.bookmark(0).await.unwrap(), FeedPosition::START);
    assert_eq!(store.appended("ev-1").await.unwrap(), None);
    assert!(
        store
            .document_entries("Invoice", "INV-1")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.site_checkpoint().await.unwrap(), None);

    // A bookmark is per lane, and the latest value wins (also when it moves to a new generation).
    store
        .set_bookmark(0, FeedPosition::new(1, 10))
        .await
        .unwrap();
    store
        .set_bookmark(1, FeedPosition::new(1, 20))
        .await
        .unwrap();
    assert_eq!(store.bookmark(0).await.unwrap(), FeedPosition::new(1, 10));
    assert_eq!(store.bookmark(1).await.unwrap(), FeedPosition::new(1, 20));
    assert_eq!(store.bookmark(2).await.unwrap(), FeedPosition::START);
    store
        .set_bookmark(0, FeedPosition::new(1, 11))
        .await
        .unwrap();
    assert_eq!(store.bookmark(0).await.unwrap(), FeedPosition::new(1, 11));
    store
        .set_bookmark(0, FeedPosition::new(2, 1))
        .await
        .unwrap();
    assert_eq!(store.bookmark(0).await.unwrap(), FeedPosition::new(2, 1));

    // An event id remembers where it was chained; other ids are unaffected.
    store.record_appended("ev-1", 3, 41).await.unwrap();
    store.record_appended("ev-2", 0, 7).await.unwrap();
    assert_eq!(store.appended("ev-1").await.unwrap(), Some((3, 41)));
    assert_eq!(store.appended("ev-2").await.unwrap(), Some((0, 7)));
    assert_eq!(store.appended("ev-3").await.unwrap(), None);

    // A document's entries come back in the order they were recorded; documents and doctypes are
    // separate keys; recording an entry again changes nothing.
    store
        .record_document_entry("Invoice", "INV-1", 2, 5)
        .await
        .unwrap();
    store
        .record_document_entry("Invoice", "INV-1", 2, 9)
        .await
        .unwrap();
    store
        .record_document_entry("Invoice", "INV-1", 2, 5)
        .await
        .unwrap();
    store
        .record_document_entry("Invoice", "INV-2", 1, 3)
        .await
        .unwrap();
    store
        .record_document_entry("Bill", "INV-1", 0, 1)
        .await
        .unwrap();
    assert_eq!(
        store.document_entries("Invoice", "INV-1").await.unwrap(),
        vec![(2, 5), (2, 9)]
    );
    assert_eq!(
        store.document_entries("Invoice", "INV-2").await.unwrap(),
        vec![(1, 3)]
    );
    assert_eq!(
        store.document_entries("Bill", "INV-1").await.unwrap(),
        vec![(0, 1)]
    );
    assert!(
        store
            .document_entries("Bill", "INV-2")
            .await
            .unwrap()
            .is_empty()
    );

    // The site checkpoint is the newest one set.
    let first = checkpoint(site, 100);
    let second = checkpoint(site, 200);
    store.set_site_checkpoint(first.clone()).await.unwrap();
    assert_eq!(store.site_checkpoint().await.unwrap(), Some(first));
    store.set_site_checkpoint(second.clone()).await.unwrap();
    assert_eq!(store.site_checkpoint().await.unwrap(), Some(second));
}

#[tokio::test]
async fn the_contract_holds_for_the_in_memory_store() {
    contract(&InMemoryAppenderStore::new(), "site-a").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_contract_holds_for_the_postgres_store() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    contract(&fx.store("site-a"), "site-a").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_resumes_the_bookmark_and_the_indexes() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    let before = fx.store("site-a");
    before
        .set_bookmark(4, FeedPosition::new(3, 99))
        .await
        .unwrap();
    before.record_appended("ev-9", 4, 12).await.unwrap();
    before
        .record_document_entry("Invoice", "INV-9", 4, 12)
        .await
        .unwrap();
    before
        .set_site_checkpoint(checkpoint("site-a", 50))
        .await
        .unwrap();
    drop(before);

    // A new store over a new pool, the same database: a restarted process.
    let after = fx.store("site-a");
    assert_eq!(after.bookmark(4).await.unwrap(), FeedPosition::new(3, 99));
    assert_eq!(after.appended("ev-9").await.unwrap(), Some((4, 12)));
    assert_eq!(
        after.document_entries("Invoice", "INV-9").await.unwrap(),
        vec![(4, 12)]
    );
    assert_eq!(
        after.site_checkpoint().await.unwrap(),
        Some(checkpoint("site-a", 50))
    );
}

fn event(id: &str) -> DocAuditEvent {
    DocAuditEvent {
        site: "site-a".to_owned(),
        doctype: "invoice".to_owned(),
        id: id.to_owned(),
        version: 1,
        operation: DocOperation::Create,
        actor: ActorRef {
            actor: format!("user:{}", RecordId::new_v7()),
            on_behalf_of: None,
            component: None,
        },
        at: 1_700_000_000_000_000,
        changed_fields: vec![],
        diff: vec![],
        hidden_changes: false,
        command_id: None,
        source: AuditSource::Api,
        truncated: false,
        diff_digest: None,
        method: None,
        correction_half: None,
        via: None,
        node: None,
        outcome: None,
        error_class: None,
        error_code: None,
    }
}

/// An outbox held in memory, served as a feed.
struct Outbox(Vec<OutboxRecord>);

impl Outbox {
    /// `n` audit events about `n` documents, positions `(1, 1)` to `(1, n)`.
    fn of(n: u64) -> Outbox {
        Outbox(
            (1..=n)
                .map(|i| OutboxRecord {
                    position: FeedPosition::new(1, i),
                    transaction: RecordId::new_v7(),
                    ordinal: 0,
                    event: outbox_event_of(&event(&RecordId::new_v7().to_string())).unwrap(),
                })
                .collect(),
        )
    }
}

impl OutboxFeed for Outbox {
    fn read<'a>(
        &'a self,
        after: FeedPosition,
        limit: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<OutboxRecord>, DocAuditError>> + Send + 'a>> {
        Box::pin(async move {
            Ok(self
                .0
                .iter()
                .filter(|r| r.position > after)
                .take(limit as usize)
                .cloned()
                .collect())
        })
    }
}

fn appender<S: AuditStore>(
    chain: &Arc<S>,
    state: Arc<dyn AppenderStore>,
    lane: u32,
) -> LaneAppender<S> {
    LaneAppender::new(
        Arc::clone(chain),
        "site-a",
        lane,
        LaneConfig::new(1, 1),
        signer(),
        state,
    )
}

/// The defect reproduction, current-thread flavour: a real appender over the PostgreSQL chain and
/// the PostgreSQL state drains an outbox inside a runtime thread, which the blocking bridge
/// refused with `pg_bridge.wrong_thread`.
#[tokio::test(flavor = "current_thread")]
async fn a_real_appender_drains_inside_a_current_thread_runtime() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    let chain = Arc::new(fx.chain().await);
    let state: Arc<dyn AppenderStore> = Arc::new(fx.store("site-a"));
    let lane = appender(&chain, Arc::clone(&state), 0);
    let outbox = Outbox::of(4);

    let report = lane.drain(&outbox, 100).await.expect("the drain works");
    assert_eq!(report.appended, 4);
    assert_eq!(
        state.bookmark(0).await.unwrap(),
        FeedPosition::new(1, 4),
        "the bookmark followed the last chained event"
    );
    let entries = chain
        .range(&lane_site("site-a", 0), 1, u64::MAX)
        .await
        .unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(lane.drain(&outbox, 100).await.unwrap().appended, 0);
}

/// And the multi-thread flavour, from a spawned task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_appender_drains_inside_a_multi_thread_runtime() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    let chain = Arc::new(fx.chain().await);
    let state: Arc<dyn AppenderStore> = Arc::new(fx.store("site-a"));
    let lane = Arc::new(appender(&chain, state, 0));
    let outbox = Arc::new(Outbox::of(4));
    let (task_lane, task_outbox) = (Arc::clone(&lane), Arc::clone(&outbox));
    let report = tokio::spawn(async move { task_lane.drain(&*task_outbox, 100).await })
        .await
        .unwrap()
        .expect("the drain works");
    assert_eq!(report.appended, 4);
    let entries = chain
        .range(&lane_site("site-a", 0), 1, u64::MAX)
        .await
        .unwrap();
    assert_eq!(entries.len(), 4);
}

/// A restart: a new appender and state over the same database resumes from the bookmark and
/// chains only the events that arrived since.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_appender_resumes_from_the_bookmark() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    let chain = Arc::new(fx.chain().await);
    let first = appender(&chain, Arc::new(fx.store("site-a")), 0);
    assert_eq!(first.drain(&Outbox::of(2), 100).await.unwrap().appended, 2);
    drop(first);

    let state: Arc<dyn AppenderStore> = Arc::new(fx.store("site-a"));
    assert_eq!(state.bookmark(0).await.unwrap(), FeedPosition::new(1, 2));
    let second = appender(&chain, state, 0);
    let report = second.drain(&Outbox::of(5), 100).await.unwrap();
    assert_eq!(report.appended, 3, "only the new events are chained");
    let entries = chain
        .range(&lane_site("site-a", 0), 1, u64::MAX)
        .await
        .unwrap();
    assert_eq!(entries.len(), 5);
}

/// The crash the old brief named: the chain append and the index writes are durable, the bookmark
/// is not. The restarted appender replays the event and chains nothing a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_between_the_chain_append_and_the_bookmark_replays_without_a_duplicate() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    let chain = Arc::new(InMemoryStore::new());
    let first = appender(&chain, Arc::new(fx.store("site-a")), 0);
    let result = first
        .append_event("ev-1", &event("INV-1"))
        .await
        .expect("append");
    assert_eq!(result, AppendResult::Appended { seq: 1 });
    // The process dies here: `drain` would now have stored the bookmark. It did not.
    drop(first);

    let state = Arc::new(fx.store("site-a"));
    assert_eq!(
        state.bookmark(0).await.unwrap(),
        FeedPosition::START,
        "the bookmark never advanced"
    );
    let second = appender(&chain, Arc::clone(&state) as Arc<dyn AppenderStore>, 0);
    let replay = second
        .append_event("ev-1", &event("INV-1"))
        .await
        .expect("replay");
    assert_eq!(replay, AppendResult::AlreadyChained { lane: 0, seq: 1 });
    let entries = chain
        .range(&lane_site("site-a", 0), 1, u64::MAX)
        .await
        .expect("range");
    assert_eq!(entries.len(), 1, "the event is chained once");
    assert_eq!(
        state.document_entries("invoice", "INV-1").await.unwrap(),
        vec![(0, 1)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sites_in_one_database_never_see_each_other() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    let a = fx.store("site-a");
    let b = fx.store("site-b");
    a.set_bookmark(0, FeedPosition::new(1, 5)).await.unwrap();
    a.record_appended("ev-1", 0, 1).await.unwrap();
    a.record_document_entry("Invoice", "INV-1", 0, 1)
        .await
        .unwrap();
    a.set_site_checkpoint(checkpoint("site-a", 10))
        .await
        .unwrap();
    assert_eq!(b.bookmark(0).await.unwrap(), FeedPosition::START);
    assert_eq!(b.appended("ev-1").await.unwrap(), None);
    assert!(
        b.document_entries("Invoice", "INV-1")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(b.site_checkpoint().await.unwrap(), None);

    // The same event id and bookmark lane in site B are B's own, and A is unchanged.
    b.set_bookmark(0, FeedPosition::new(9, 9)).await.unwrap();
    b.record_appended("ev-1", 5, 77).await.unwrap();
    b.set_site_checkpoint(checkpoint("site-b", 20))
        .await
        .unwrap();
    assert_eq!(a.bookmark(0).await.unwrap(), FeedPosition::new(1, 5));
    assert_eq!(b.bookmark(0).await.unwrap(), FeedPosition::new(9, 9));
    assert_eq!(a.appended("ev-1").await.unwrap(), Some((0, 1)));
    assert_eq!(b.appended("ev-1").await.unwrap(), Some((5, 77)));
    assert_eq!(
        a.site_checkpoint().await.unwrap(),
        Some(checkpoint("site-a", 10))
    );
    assert_eq!(
        b.site_checkpoint().await.unwrap(),
        Some(checkpoint("site-b", 20))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_writes_change_nothing_and_a_different_value_for_an_event_id_is_refused() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    let store = fx.store("site-a");
    store.record_appended("ev-1", 2, 8).await.unwrap();
    store.record_appended("ev-1", 2, 8).await.unwrap();
    store
        .record_document_entry("Invoice", "INV-1", 2, 8)
        .await
        .unwrap();
    store
        .record_document_entry("Invoice", "INV-1", 2, 8)
        .await
        .unwrap();
    store
        .set_bookmark(2, FeedPosition::new(1, 4))
        .await
        .unwrap();
    store
        .set_bookmark(2, FeedPosition::new(1, 4))
        .await
        .unwrap();
    assert_eq!(fx.count(EVENT_INDEX_TABLE).await, 1);
    assert_eq!(fx.count("spot_audit_doc_index").await, 1);
    assert_eq!(fx.count(BOOKMARK_TABLE).await, 1);

    for (lane, seq) in [(2, 9), (3, 8)] {
        let error = store.record_appended("ev-1", lane, seq).await.unwrap_err();
        assert_eq!(error.code(), APPENDER_STATE);
        assert!(error.to_string().contains(EVENT_CONFLICT), "{error}");
    }
    assert_eq!(
        store.appended("ev-1").await.unwrap(),
        Some((2, 8)),
        "never overwritten"
    );

    // An older site checkpoint never replaces a newer one.
    store
        .set_site_checkpoint(checkpoint("site-a", 300))
        .await
        .unwrap();
    store
        .set_site_checkpoint(checkpoint("site-a", 100))
        .await
        .unwrap();
    assert_eq!(
        store.site_checkpoint().await.unwrap(),
        Some(checkpoint("site-a", 300))
    );
}

/// A database that is not there: the drain fails with `docaudit.appender_state`, nothing is
/// chained, the stored bookmark is unchanged, and once the pool is healthy the same events are
/// chained once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unavailable_database_fails_the_drain_and_loses_nothing() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    let chain = Arc::new(fx.chain().await);
    let outbox = Outbox::of(3);

    let dead: Arc<dyn AppenderStore> = Arc::new(
        PgAppenderStore::new(nowhere_pool(), "site-a").with_deadline(Duration::from_millis(400)),
    );
    let failing = appender(&chain, dead, 0);
    let error = failing
        .drain(&outbox, 100)
        .await
        .expect_err("the database is gone");
    assert_eq!(error.code(), APPENDER_STATE);
    let text = error.to_string();
    assert!(
        text.contains("bookmark") && text.contains("auditstore.unavailable"),
        "{text}"
    );
    assert!(
        chain
            .range(&lane_site("site-a", 0), 1, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fx.store("site-a").bookmark(0).await.unwrap(),
        FeedPosition::START,
        "the stored bookmark did not move"
    );

    // The database comes back: the same events are chained once.
    let healthy = appender(&chain, Arc::new(fx.store("site-a")), 0);
    assert_eq!(healthy.drain(&outbox, 100).await.unwrap().appended, 3);
    assert_eq!(
        chain
            .range(&lane_site("site-a", 0), 1, 100)
            .await
            .unwrap()
            .len(),
        3
    );
}

/// A failure in the middle of a drain: the bookmark write of the second event is refused by the
/// database, the stored bookmark stays at the first, and after repair the second is found already
/// chained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_bookmark_write_stops_the_drain_and_the_retry_chains_each_event_once() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    let chain = Arc::new(fx.chain().await);
    let state: Arc<dyn AppenderStore> = Arc::new(fx.store("site-a"));
    let lane = appender(&chain, Arc::clone(&state), 0);
    let both = Outbox::of(2);
    let first_only = Outbox(both.0[..1].to_vec());
    lane.drain(&first_only, 100).await.unwrap();

    // The database refuses every bookmark write; the second event is chained, its bookmark is not.
    fx.exec(
        "CREATE FUNCTION spot_test_refuse() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'refused'; END $$; \
         CREATE TRIGGER spot_test_refuse BEFORE INSERT OR UPDATE ON spot_audit_bookmark \
         FOR EACH ROW EXECUTE FUNCTION spot_test_refuse()",
    )
    .await;
    let error = lane.drain(&both, 100).await.expect_err("the state refuses");
    assert_eq!(error.code(), APPENDER_STATE);
    assert!(error.to_string().contains("set_bookmark"), "{error}");
    fx.exec("DROP TRIGGER spot_test_refuse ON spot_audit_bookmark")
        .await;
    assert_eq!(
        state.bookmark(0).await.unwrap(),
        FeedPosition::new(1, 1),
        "the bookmark never advanced past the stored index"
    );

    let report = lane.drain(&both, 100).await.unwrap();
    assert_eq!(report.appended, 0, "the second event was already chained");
    let entries = chain.range(&lane_site("site-a", 0), 1, 100).await.unwrap();
    assert_eq!(entries.len(), 2, "each event once");
}

/// No key, document value or actor reaches an error text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_value_appears_in_an_error() {
    let Some(fx) = Fixture::with_tables().await else {
        return;
    };
    let store = fx.store("site-secret-site");
    let marks = [
        "secret-event-id",
        "secret-doctype",
        "secret-doc-id",
        "secret-actor",
    ];
    fx.exec("DROP TABLE spot_audit_event_index; DROP TABLE spot_audit_doc_index; DROP TABLE spot_audit_bookmark; DROP TABLE spot_audit_site_checkpoint").await;
    let mut cp = checkpoint("site-secret-site", 5);
    cp.signed.key_id = marks[3].to_owned();
    let errors = vec![
        store.appended(marks[0]).await.unwrap_err(),
        store.record_appended(marks[0], 1, 1).await.unwrap_err(),
        store
            .document_entries(marks[1], marks[2])
            .await
            .unwrap_err(),
        store
            .record_document_entry(marks[1], marks[2], 1, 1)
            .await
            .unwrap_err(),
        store.bookmark(1).await.unwrap_err(),
        store.site_checkpoint().await.unwrap_err(),
        store.set_site_checkpoint(cp).await.unwrap_err(),
    ];
    for error in &errors {
        let text = format!("{error} {error:?}");
        for mark in marks.iter().chain(["site-secret-site"].iter()) {
            assert!(!text.contains(mark), "{text} names {mark}");
        }
        assert_eq!(error.code(), APPENDER_STATE);
        assert!(text.contains("auditstore.unavailable"), "{text}");
    }
    // A conflict names no id either.
    fx.exec(spot_audit_store::EVENT_INDEX_TABLE_DDL).await;
    store.record_appended(marks[0], 1, 1).await.unwrap();
    let conflict = store.record_appended(marks[0], 2, 2).await.unwrap_err();
    let text = format!("{conflict} {conflict:?}");
    assert!(
        !text.contains(marks[0]) && !text.contains("site-secret-site"),
        "{text}"
    );
}

fn migrator_options() -> PlanOptions {
    PlanOptions {
        extra_tables: TABLE_DDL
            .iter()
            .chain(APPENDER_TABLE_DDL.iter())
            .map(|(table, statement)| {
                ExtraTable::new(*table, vec![(*statement).to_owned()]).unwrap()
            })
            .collect(),
        ..PlanOptions::default()
    }
}

/// The migrator plans and applies the four tables with the chain's, a second plan is empty, and
/// the presence check reports each state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_migrator_creates_the_tables_and_verify_reports_them() {
    let Some(fx) = Fixture::bare().await else {
        return;
    };
    let catalog = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    let backend = PgBackend::connect(fx.db.config()).await.unwrap();
    let pool = backend.pool();

    let missing = verify_appender(&pool).await.unwrap();
    assert_eq!(missing.missing().len(), 4);
    assert!(!missing.is_complete());
    assert_eq!(
        missing.require_complete().unwrap_err().code,
        APPENDER_TABLES_MISSING
    );

    let plan = dry_run(&backend, &catalog, &migrator_options())
        .await
        .unwrap();
    assert!(plan.refused.is_empty(), "{:?}", plan.refused);
    apply(&backend, &plan, ApplyOptions::default())
        .await
        .unwrap();
    let found = verify_appender(&pool).await.unwrap();
    assert!(found.is_complete(), "{:?}", found.missing());
    found.require_complete().unwrap();

    let again = dry_run(&backend, &catalog, &migrator_options())
        .await
        .unwrap();
    assert!(
        again.steps.is_empty(),
        "a second plan has nothing to do: {:?}",
        again.steps
    );

    // A table with other columns does not count as present.
    fx.exec("ALTER TABLE spot_audit_bookmark DROP COLUMN generation")
        .await;
    let changed = verify_appender(&pool).await.unwrap();
    assert_eq!(
        changed,
        AppenderTables {
            bookmark: false,
            event_index: true,
            doc_index: true,
            site_checkpoint: true
        }
    );
    assert_eq!(changed.missing(), vec![BOOKMARK_TABLE]);

    // The store works on the migrated tables.
    fx.exec("ALTER TABLE spot_audit_bookmark ADD COLUMN generation bigint NOT NULL DEFAULT 0")
        .await;
    contract(&fx.store("site-a"), "site-a").await;
}
