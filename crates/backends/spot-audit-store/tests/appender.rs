//! The PostgreSQL appender state (WP-AI-11.T5): one contract suite run against the in-memory
//! reference and the PostgreSQL store, then restart, replay, site isolation, idempotence, loud
//! failure, the blocking-bridge thread rule, no value in an error, and the migrator's plan.
//!
//! Every test that needs a server skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`
//! and fails when `SPOT_REQUIRE_PG=1`. The tests are plain `#[test]`s: the store is synchronous
//! and the bridge refuses a call from a thread inside a tokio runtime, so a test thread must not
//! be one; a small runtime is used only to prepare and inspect the scratch database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use spot_audit_chain::{AuditStore, CheckpointSigner, InMemoryStore, LocalSigner, genesis_hash};
use spot_audit_store::{
    APPENDER_TABLE_DDL, APPENDER_TABLES_MISSING, AppenderTables, BOOKMARK_TABLE, EVENT_CONFLICT,
    EVENT_INDEX_TABLE, PgAppenderStore, TABLE_DDL, UNAVAILABLE, poison_position, verify_appender,
};
use spot_doc_audit::{
    ActorRef, AppendResult, AppenderStore, AuditSource, DocAuditEvent, DocOperation,
    InMemoryAppenderStore, LaneAppender, LaneConfig, LaneHead, SiteCheckpoint, lane_site,
};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_pg_migrate::{ApplyOptions, ExtraTable, PlanOptions, apply, dry_run};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{Catalog, Collation};

struct Fixture {
    rt: tokio::runtime::Runtime,
    db: TestDb,
}

impl Fixture {
    /// A scratch database with the appender tables created from the public DDL.
    fn with_tables() -> Option<Fixture> {
        let fx = Fixture::bare()?;
        fx.exec(
            &APPENDER_TABLE_DDL
                .iter()
                .map(|(_, ddl)| *ddl)
                .collect::<Vec<_>>()
                .join(";"),
        );
        Some(fx)
    }

    /// A scratch database with nothing in it.
    fn bare() -> Option<Fixture> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let db = rt.block_on(TestDb::create())?;
        Some(Fixture { rt, db })
    }

    fn exec(&self, sql: &str) {
        self.rt.block_on(async {
            self.db
                .raw_client()
                .await
                .batch_execute(sql)
                .await
                .expect("sql");
        });
    }

    fn count(&self, table: &str) -> i64 {
        self.rt.block_on(async {
            self.db
                .raw_client()
                .await
                .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .await
                .expect("count")
                .get(0)
        })
    }

    /// One bridge on its own pool. The pool is lazy: it opens no connection until used.
    fn bridge(&self) -> Arc<Bridge> {
        let mut config = self.db.config();
        config.pool_max_size = 8;
        let pool = PgBackend::connect_unchecked(config)
            .expect("backend")
            .pool();
        Arc::new(
            Bridge::new(
                BridgeConfig {
                    name: "audit_appender_test".to_owned(),
                    workers: 2,
                    max_in_flight: 4,
                    queue_capacity: 16,
                    default_deadline: Duration::from_secs(5),
                    max_deadline: Duration::from_secs(20),
                },
                pool,
            )
            .expect("bridge"),
        )
    }

    fn store(&self, site: &str) -> PgAppenderStore {
        PgAppenderStore::new(self.bridge(), site)
    }
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
fn contract(store: &dyn AppenderStore, site: &str) {
    // A new store: no bookmark, nothing chained, no document history, no checkpoint.
    assert_eq!(store.bookmark(0), FeedPosition::START);
    assert_eq!(store.appended("ev-1"), None);
    assert!(store.document_entries("Invoice", "INV-1").is_empty());
    assert_eq!(store.site_checkpoint(), None);

    // A bookmark is per lane, and the latest value wins (also when it moves to a new generation).
    store.set_bookmark(0, FeedPosition::new(1, 10));
    store.set_bookmark(1, FeedPosition::new(1, 20));
    assert_eq!(store.bookmark(0), FeedPosition::new(1, 10));
    assert_eq!(store.bookmark(1), FeedPosition::new(1, 20));
    assert_eq!(store.bookmark(2), FeedPosition::START);
    store.set_bookmark(0, FeedPosition::new(1, 11));
    assert_eq!(store.bookmark(0), FeedPosition::new(1, 11));
    store.set_bookmark(0, FeedPosition::new(2, 1));
    assert_eq!(store.bookmark(0), FeedPosition::new(2, 1));

    // An event id remembers where it was chained; other ids are unaffected.
    store.record_appended("ev-1", 3, 41);
    store.record_appended("ev-2", 0, 7);
    assert_eq!(store.appended("ev-1"), Some((3, 41)));
    assert_eq!(store.appended("ev-2"), Some((0, 7)));
    assert_eq!(store.appended("ev-3"), None);

    // A document's entries come back in the order they were recorded; documents and doctypes are
    // separate keys.
    store.record_document_entry("Invoice", "INV-1", 2, 5);
    store.record_document_entry("Invoice", "INV-1", 2, 9);
    store.record_document_entry("Invoice", "INV-2", 1, 3);
    store.record_document_entry("Bill", "INV-1", 0, 1);
    assert_eq!(
        store.document_entries("Invoice", "INV-1"),
        vec![(2, 5), (2, 9)]
    );
    assert_eq!(store.document_entries("Invoice", "INV-2"), vec![(1, 3)]);
    assert_eq!(store.document_entries("Bill", "INV-1"), vec![(0, 1)]);
    assert!(store.document_entries("Bill", "INV-2").is_empty());

    // The site checkpoint is the newest one set.
    let first = checkpoint(site, 100);
    let second = checkpoint(site, 200);
    store.set_site_checkpoint(first.clone());
    assert_eq!(store.site_checkpoint(), Some(first));
    store.set_site_checkpoint(second.clone());
    assert_eq!(store.site_checkpoint(), Some(second));
}

#[test]
fn the_contract_holds_for_the_in_memory_store() {
    contract(&InMemoryAppenderStore::new(), "site-a");
}

#[test]
fn the_contract_holds_for_the_postgres_store() {
    let Some(fx) = Fixture::with_tables() else {
        return;
    };
    let store = fx.store("site-a");
    contract(&store, "site-a");
    assert_eq!(store.failure(), None, "no call failed");
}

#[test]
fn a_restart_resumes_the_bookmark_and_the_indexes() {
    let Some(fx) = Fixture::with_tables() else {
        return;
    };
    let before = fx.store("site-a");
    before.set_bookmark(4, FeedPosition::new(3, 99));
    before.record_appended("ev-9", 4, 12);
    before.record_document_entry("Invoice", "INV-9", 4, 12);
    before.set_site_checkpoint(checkpoint("site-a", 50));
    drop(before);

    // A new store over a new bridge and pool, the same database: a restarted process.
    let after = fx.store("site-a");
    assert_eq!(after.bookmark(4), FeedPosition::new(3, 99));
    assert_eq!(after.appended("ev-9"), Some((4, 12)));
    assert_eq!(after.document_entries("Invoice", "INV-9"), vec![(4, 12)]);
    assert_eq!(after.site_checkpoint(), Some(checkpoint("site-a", 50)));
    assert_eq!(after.failure(), None);
}

fn event(id: &str) -> DocAuditEvent {
    DocAuditEvent {
        site: "site-a".to_owned(),
        doctype: "Invoice".to_owned(),
        id: id.to_owned(),
        version: 1,
        operation: DocOperation::Create,
        actor: ActorRef {
            actor: "user:u1".to_owned(),
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

struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs a future on this (non-runtime) thread. The chain here is in memory, so nothing needs a
/// reactor; the appender's synchronous state calls run on this thread, as the bridge requires.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::park();
    }
}

fn appender(
    chain: &Arc<InMemoryStore>,
    state: Arc<PgAppenderStore>,
    lane: u32,
) -> LaneAppender<InMemoryStore> {
    LaneAppender::new(
        Arc::clone(chain),
        "site-a",
        lane,
        LaneConfig::new(1, 1),
        signer(),
        state,
    )
}

/// The crash the brief names: the chain append and the index writes are durable, the bookmark is
/// not. The restarted appender replays the event and chains nothing a second time.
#[test]
fn a_crash_between_the_chain_append_and_the_bookmark_replays_without_a_duplicate() {
    let Some(fx) = Fixture::with_tables() else {
        return;
    };
    let chain = Arc::new(InMemoryStore::new());
    let first = appender(&chain, Arc::new(fx.store("site-a")), 0);
    let result = block_on(first.append_event("ev-1", &event("INV-1"))).expect("append");
    assert_eq!(result, AppendResult::Appended { seq: 1 });
    // The process dies here: `drain` would now have stored the bookmark. It did not.
    drop(first);

    let state = Arc::new(fx.store("site-a"));
    assert_eq!(
        state.bookmark(0),
        FeedPosition::START,
        "the bookmark never advanced"
    );
    let second = appender(&chain, Arc::clone(&state), 0);
    let replay = block_on(second.append_event("ev-1", &event("INV-1"))).expect("replay");
    assert_eq!(replay, AppendResult::AlreadyChained { lane: 0, seq: 1 });
    let entries = block_on(chain.range(&lane_site("site-a", 0), 1, u64::MAX)).expect("range");
    assert_eq!(entries.len(), 1, "the event is chained once");
    assert_eq!(state.document_entries("Invoice", "INV-1"), vec![(0, 1)]);
    assert_eq!(state.failure(), None);
}

#[test]
fn two_sites_in_one_database_never_see_each_other() {
    let Some(fx) = Fixture::with_tables() else {
        return;
    };
    let a = fx.store("site-a");
    let b = fx.store("site-b");
    a.set_bookmark(0, FeedPosition::new(1, 5));
    a.record_appended("ev-1", 0, 1);
    a.record_document_entry("Invoice", "INV-1", 0, 1);
    a.set_site_checkpoint(checkpoint("site-a", 10));
    assert_eq!(b.bookmark(0), FeedPosition::START);
    assert_eq!(b.appended("ev-1"), None);
    assert!(b.document_entries("Invoice", "INV-1").is_empty());
    assert_eq!(b.site_checkpoint(), None);

    // The same event id and bookmark lane in site B are B's own, and A is unchanged.
    b.set_bookmark(0, FeedPosition::new(9, 9));
    b.record_appended("ev-1", 5, 77);
    b.set_site_checkpoint(checkpoint("site-b", 20));
    assert_eq!(a.bookmark(0), FeedPosition::new(1, 5));
    assert_eq!(b.bookmark(0), FeedPosition::new(9, 9));
    assert_eq!(a.appended("ev-1"), Some((0, 1)));
    assert_eq!(b.appended("ev-1"), Some((5, 77)));
    assert_eq!(a.site_checkpoint(), Some(checkpoint("site-a", 10)));
    assert_eq!(b.site_checkpoint(), Some(checkpoint("site-b", 20)));
    assert_eq!(a.failure(), None);
    assert_eq!(b.failure(), None);
}

#[test]
fn repeated_writes_change_nothing_and_a_different_value_for_an_event_id_is_refused() {
    let Some(fx) = Fixture::with_tables() else {
        return;
    };
    let store = fx.store("site-a");
    store.try_record_appended("ev-1", 2, 8).unwrap();
    store.try_record_appended("ev-1", 2, 8).unwrap();
    store
        .try_record_document_entry("Invoice", "INV-1", 2, 8)
        .unwrap();
    store
        .try_record_document_entry("Invoice", "INV-1", 2, 8)
        .unwrap();
    store.try_set_bookmark(2, FeedPosition::new(1, 4)).unwrap();
    store.try_set_bookmark(2, FeedPosition::new(1, 4)).unwrap();
    assert_eq!(fx.count(EVENT_INDEX_TABLE), 1);
    assert_eq!(fx.count("spot_audit_doc_index"), 1);
    assert_eq!(fx.count(BOOKMARK_TABLE), 1);

    for (lane, seq) in [(2, 9), (3, 8)] {
        let error = store.try_record_appended("ev-1", lane, seq).unwrap_err();
        assert_eq!(error.code, EVENT_CONFLICT);
    }
    assert_eq!(
        store.try_appended("ev-1").unwrap(),
        Some((2, 8)),
        "never overwritten"
    );

    // Through the trait the refusal latches: the appender cannot go on past a conflicting index.
    store.record_appended("ev-1", 5, 5);
    assert_eq!(store.failure().map(|e| e.code), Some(EVENT_CONFLICT));
    assert_eq!(store.bookmark(0), poison_position());
    assert_eq!(fx.count(EVENT_INDEX_TABLE), 1);

    // An older site checkpoint never replaces a newer one.
    store.take_failure();
    store
        .try_set_site_checkpoint(&checkpoint("site-a", 300))
        .unwrap();
    store
        .try_set_site_checkpoint(&checkpoint("site-a", 100))
        .unwrap();
    assert_eq!(
        store.try_site_checkpoint().unwrap(),
        Some(checkpoint("site-a", 300))
    );
}

/// A database failure is never answered as "not found": the store latches it and fails closed.
#[test]
fn a_database_failure_is_latched_and_fails_closed() {
    let Some(fx) = Fixture::with_tables() else {
        return;
    };
    let store = fx.store("site-a");
    store.set_bookmark(0, FeedPosition::new(1, 3));
    store.record_appended("ev-1", 0, 1);
    assert_eq!(store.failure(), None);

    fx.exec("ALTER TABLE spot_audit_event_index RENAME TO spot_audit_event_index_gone");
    // The index cannot be read: the answer is not "never chained", and the error has a code.
    assert_eq!(store.try_appended("ev-1").unwrap_err().code, UNAVAILABLE);
    assert_eq!(store.appended("ev-1"), Some((u32::MAX, u64::MAX)));
    assert_eq!(store.failure().map(|e| e.code), Some(UNAVAILABLE));

    // Latched: the bookmark is a position no feed accepts, and nothing is written.
    assert_eq!(store.bookmark(0), poison_position());
    store.set_bookmark(0, FeedPosition::new(1, 99));
    store.record_document_entry("Invoice", "INV-1", 0, 1);
    assert_eq!(fx.count("spot_audit_doc_index"), 0);
    assert_eq!(fx.count(BOOKMARK_TABLE), 1);

    // Repaired and cleared: the stored bookmark is what it was before the failure.
    fx.exec("ALTER TABLE spot_audit_event_index_gone RENAME TO spot_audit_event_index");
    assert_eq!(store.take_failure().map(|e| e.code), Some(UNAVAILABLE));
    assert_eq!(
        store.bookmark(0),
        FeedPosition::new(1, 3),
        "the bookmark never advanced"
    );
    assert_eq!(store.appended("ev-1"), Some((0, 1)));
    assert_eq!(store.failure(), None);
}

/// A bookmark write that fails leaves the bookmark where it was and fails the next drain.
#[test]
fn a_failed_write_latches_so_the_next_drain_fails() {
    let Some(fx) = Fixture::with_tables() else {
        return;
    };
    let store = fx.store("site-a");
    fx.exec("DROP TABLE spot_audit_doc_index");
    store.record_document_entry("Invoice", "INV-1", 0, 1);
    assert_eq!(store.failure().map(|e| e.code), Some(UNAVAILABLE));
    store.set_bookmark(0, FeedPosition::new(1, 50));
    assert_eq!(
        fx.count(BOOKMARK_TABLE),
        0,
        "the bookmark is not stored past a failed index write"
    );
    assert_eq!(store.bookmark(0), poison_position());
}

/// The store is not callable from a request path: a thread inside a tokio runtime is refused.
#[test]
fn the_bridge_refuses_the_store_on_an_async_thread() {
    let Some(fx) = Fixture::with_tables() else {
        return;
    };
    let store = Arc::new(fx.store("site-a"));
    let inside = Arc::clone(&store);
    let (refused, appended, bookmark) = fx.rt.block_on(async move {
        let refused = inside.try_bookmark(0).unwrap_err();
        let appended = inside.appended("ev-1");
        let bookmark = inside.bookmark(0);
        (refused, appended, bookmark)
    });
    assert_eq!(refused.code, spot_pg_bridge::WRONG_THREAD);
    assert_eq!(
        appended,
        Some((u32::MAX, u64::MAX)),
        "refused, not answered"
    );
    assert_eq!(bookmark, poison_position());
    assert_eq!(
        store.failure().map(|e| e.code),
        Some(spot_pg_bridge::WRONG_THREAD)
    );
    // The same store works from an ordinary thread once the failure is cleared.
    store.take_failure();
    assert_eq!(store.bookmark(0), FeedPosition::START);
}

/// No key, document value or actor reaches an error text.
#[test]
fn no_value_appears_in_an_error() {
    let Some(fx) = Fixture::with_tables() else {
        return;
    };
    let store = fx.store("site-secret-site");
    let marks = [
        "secret-event-id",
        "secret-doctype",
        "secret-doc-id",
        "secret-actor",
    ];
    fx.exec("DROP TABLE spot_audit_event_index; DROP TABLE spot_audit_doc_index; DROP TABLE spot_audit_bookmark; DROP TABLE spot_audit_site_checkpoint");
    let errors = vec![
        store.try_appended(marks[0]).unwrap_err(),
        store.try_record_appended(marks[0], 1, 1).unwrap_err(),
        store.try_document_entries(marks[1], marks[2]).unwrap_err(),
        store
            .try_record_document_entry(marks[1], marks[2], 1, 1)
            .unwrap_err(),
        store.try_bookmark(1).unwrap_err(),
        store.try_site_checkpoint().unwrap_err(),
        {
            let mut cp = checkpoint("site-secret-site", 5);
            cp.signed.key_id = marks[3].to_owned();
            store.try_set_site_checkpoint(&cp).unwrap_err()
        },
    ];
    for error in &errors {
        let text = format!("{error} {error:?}");
        for mark in marks.iter().chain(["site-secret-site"].iter()) {
            assert!(!text.contains(mark), "{text} names {mark}");
        }
        assert_eq!(error.code, UNAVAILABLE);
    }
    // A conflict names no id either.
    fx.exec(spot_audit_store_ddl_event_index());
    store.try_record_appended(marks[0], 1, 1).unwrap();
    let conflict = store.try_record_appended(marks[0], 2, 2).unwrap_err();
    let text = format!("{conflict} {conflict:?}");
    assert!(!text.contains(marks[0]));
    // The wrong-thread refusal as well.
    let refused = fx
        .rt
        .block_on(async { store.try_appended(marks[0]).unwrap_err() });
    let text = format!("{refused} {refused:?}");
    assert!(!text.contains(marks[0]) && !text.contains("site-secret-site"));
}

fn spot_audit_store_ddl_event_index() -> &'static str {
    spot_audit_store::EVENT_INDEX_TABLE_DDL
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
#[test]
fn the_migrator_creates_the_tables_and_verify_reports_them() {
    let Some(fx) = Fixture::bare() else { return };
    let catalog = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    let backend = fx.rt.block_on(PgBackend::connect(fx.db.config())).unwrap();
    let pool = backend.pool();

    let missing = fx.rt.block_on(verify_appender(&pool)).unwrap();
    assert_eq!(missing.missing().len(), 4);
    assert!(!missing.is_complete());
    assert_eq!(
        missing.require_complete().unwrap_err().code,
        APPENDER_TABLES_MISSING
    );

    let plan = fx
        .rt
        .block_on(dry_run(&backend, &catalog, &migrator_options()))
        .unwrap();
    assert!(plan.refused.is_empty(), "{:?}", plan.refused);
    fx.rt
        .block_on(apply(&backend, &plan, ApplyOptions::default()))
        .unwrap();
    let found = fx.rt.block_on(verify_appender(&pool)).unwrap();
    assert!(found.is_complete(), "{:?}", found.missing());
    found.require_complete().unwrap();

    let again = fx
        .rt
        .block_on(dry_run(&backend, &catalog, &migrator_options()))
        .unwrap();
    assert!(
        again.steps.is_empty(),
        "a second plan has nothing to do: {:?}",
        again.steps
    );

    // A table with other columns does not count as present.
    fx.exec("ALTER TABLE spot_audit_bookmark DROP COLUMN generation");
    let changed = fx.rt.block_on(verify_appender(&pool)).unwrap();
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
    fx.exec("ALTER TABLE spot_audit_bookmark ADD COLUMN generation bigint NOT NULL DEFAULT 0");
    let store = fx.store("site-a");
    contract(&store, "site-a");
}
