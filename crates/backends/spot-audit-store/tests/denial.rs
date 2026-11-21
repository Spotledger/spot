//! The PostgreSQL denial writer (WP-AI-11.T6): one outbox event per refused operation, in its own
//! transaction, scoped to its site, with no value and no message in the event, accepted by the
//! lane appender over the PostgreSQL appender state.
//!
//! Every test skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL` and fails when
//! `SPOT_REQUIRE_PG=1`. The tests are plain `#[test]`s over one small runtime that runs the
//! asynchronous parts (the appender state is an async port over the pool since WP-SEC-06.T6).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_audit_chain::{AuditStore, CheckpointSigner, InMemoryStore, LocalSigner};
use spot_audit_store::{APPENDER_TABLE_DDL, PgAppenderStore, PgDenialWriter, SITE_MISMATCH};
use spot_doc_audit::step::AUDIT_TOPIC;
use spot_doc_audit::{
    ActorRef, AuditSource, DenialWriter, DocAuditError, DocAuditEvent, DocOperation, LaneAppender,
    LaneConfig, OutboxFeed, decode_outbox, lane_site,
};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::contract_tests;
use spot_storage_contract::{
    Catalog, ChangeFeed, FeedPosition, OutboxRecord, RecordId, StorageBackend,
};

const SECRET: &str = "TAX-9981-7765-CONFIDENTIAL";

struct Fixture {
    rt: tokio::runtime::Runtime,
    db: TestDb,
    backend: Arc<PgBackend>,
    catalog: Arc<Catalog>,
}

impl Fixture {
    /// A scratch database with the contract catalog prepared (it has the outbox), a backend whose
    /// pool holds at most four connections.
    fn new() -> Option<Fixture> {
        Fixture::build()
    }

    fn build() -> Option<Fixture> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let db = rt.block_on(TestDb::create())?;
        let catalog = Arc::new(contract_tests::catalog().unwrap());
        let mut config = db.config();
        config.pool_max_size = 4;
        let backend = Arc::new(rt.block_on(PgBackend::connect(config)).expect("backend"));
        rt.block_on(backend.prepare(&catalog)).expect("prepare");
        Some(Fixture {
            rt,
            db,
            backend,
            catalog,
        })
    }

    fn writer(&self, site: &str) -> PgDenialWriter<PgBackend> {
        PgDenialWriter::new(site, Arc::clone(&self.backend), Arc::clone(&self.catalog))
    }

    fn audits(&self) -> Vec<OutboxRecord> {
        self.rt
            .block_on(self.backend.read_outbox(FeedPosition::START, 10_000))
            .expect("outbox")
            .into_iter()
            .filter(|r| r.event.topic.as_str() == AUDIT_TOPIC)
            .collect()
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

    /// A pool of its own over the scratch database (lazy: no connection until used).
    fn pool(&self) -> deadpool_postgres::Pool {
        let mut config = self.db.config();
        config.pool_max_size = 4;
        PgBackend::connect_unchecked(config)
            .expect("backend")
            .pool()
    }
}

/// A denial event: class and code only, no diff, no message, no value.
fn denial(site: &str) -> DocAuditEvent {
    DocAuditEvent {
        site: site.to_owned(),
        doctype: "customer".to_owned(),
        id: RecordId::new_v7().to_string(),
        version: 0,
        operation: DocOperation::Update,
        actor: ActorRef {
            actor: format!("user:{}", RecordId::new_v7()),
            on_behalf_of: None,
            component: None,
        },
        at: 1_700_000_000_000_000,
        changed_fields: Vec::new(),
        diff: Vec::new(),
        hidden_changes: false,
        command_id: None,
        source: AuditSource::Api,
        truncated: false,
        diff_digest: None,
        method: None,
        correction_half: None,
        via: None,
        node: None,
        outcome: Some("denied".to_owned()),
        error_class: Some("permission_denied".to_owned()),
        error_code: Some("permission.denied".to_owned()),
    }
}

#[test]
fn a_denial_is_readable_from_the_outbox_with_class_and_code_only() {
    let Some(fx) = Fixture::new() else {
        return;
    };
    let writer = fx.writer("site-a");
    let event = denial("site-a");
    fx.rt.block_on(writer.write(&event)).expect("write");

    let audits = fx.audits();
    assert_eq!(audits.len(), 1, "exactly one denial row");
    let stored = decode_outbox(&audits[0]).expect("decode");
    assert_eq!(stored, event);
    assert_eq!(stored.outcome.as_deref(), Some("denied"));
    assert_eq!(stored.doctype, "customer");
    assert_eq!(stored.operation, DocOperation::Update);
    assert_eq!(stored.error_class.as_deref(), Some("permission_denied"));
    assert_eq!(stored.error_code.as_deref(), Some("permission.denied"));
    // Nothing else: no message, no field value, no diff.
    assert!(stored.diff.is_empty() && stored.changed_fields.is_empty());
    assert!(!stored.hidden_changes && !stored.truncated && stored.diff_digest.is_none());
    let payload = &audits[0].event.payload;
    for needle in ["message", SECRET, "\"before\"", "\"after\""] {
        assert!(!payload.contains(needle), "`{needle}` in {payload}");
    }
}

#[test]
fn a_failing_database_returns_an_error_without_statement_text_or_event_fields() {
    let Some(fx) = Fixture::new() else {
        return;
    };
    let writer = fx.writer("site-a");
    let event = denial("site-a");
    // The outbox disappears under the writer: the database fails the write.
    fx.exec("ALTER TABLE spot_outbox RENAME TO spot_outbox_gone");
    let err = fx.rt.block_on(writer.write(&event)).expect_err("fails");
    let DocAuditError::OutboxWrite(message) = &err else {
        panic!("expected an outbox write error, got {err:?}");
    };
    assert!(message.contains("storage error"), "{message}");
    for needle in [
        event.id.as_str(),
        event.actor.actor.as_str(),
        "customer",
        "INSERT",
        "SELECT",
        "relation",
        "permission.denied",
    ] {
        assert!(!message.contains(needle), "`{needle}` in `{message}`");
        assert!(
            !format!("{err:?}").contains(needle),
            "`{needle}` in {err:?}"
        );
    }
    // And no partial row: with the table back, the outbox holds nothing.
    fx.exec("ALTER TABLE spot_outbox_gone RENAME TO spot_outbox");
    assert!(fx.audits().is_empty(), "a failed write leaves no row");
}

#[test]
fn two_sites_in_one_database_each_write_only_their_own() {
    let Some(fx) = Fixture::new() else {
        return;
    };
    let (a, b) = (fx.writer("site-a"), fx.writer("site-b"));
    let (ea, eb) = (denial("site-a"), denial("site-b"));
    fx.rt.block_on(a.write(&ea)).expect("a writes a");
    fx.rt.block_on(b.write(&eb)).expect("b writes b");

    // A writer refuses another site's event with the stable code and writes nothing.
    let before = fx.audits().len();
    let err = fx.rt.block_on(a.write(&eb)).expect_err("foreign event");
    let DocAuditError::OutboxWrite(message) = &err else {
        panic!("expected an outbox write error, got {err:?}");
    };
    assert!(message.starts_with(SITE_MISMATCH), "{message}");
    assert!(!message.contains("site-b") && !message.contains(eb.id.as_str()));
    assert_eq!(fx.audits().len(), before, "nothing was written");

    let sites: Vec<String> = fx
        .audits()
        .iter()
        .map(|r| decode_outbox(r).unwrap().site)
        .collect();
    assert_eq!(sites, vec!["site-a".to_owned(), "site-b".to_owned()]);
}

#[test]
fn a_flood_of_concurrent_writes_on_a_four_connection_pool_all_land_once() {
    let Some(fx) = Fixture::new() else {
        return;
    };
    let writer = Arc::new(fx.writer("site-a"));
    let events: Vec<DocAuditEvent> = (0..100).map(|_| denial("site-a")).collect();
    let ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
    fx.rt.block_on(async {
        let mut tasks = Vec::new();
        for event in events {
            let writer = Arc::clone(&writer);
            tasks.push(tokio::spawn(async move {
                writer.write(&event).await.map(|()| event.id)
            }));
        }
        for task in tasks {
            task.await.expect("task").expect("write");
        }
    });
    // The pool is four connections: the calls queued on it, and each landed exactly once.
    let mut stored: Vec<String> = fx
        .audits()
        .iter()
        .map(|r| decode_outbox(r).unwrap().id)
        .collect();
    stored.sort();
    let mut expected = ids;
    expected.sort();
    assert_eq!(stored, expected);
}

/// The outbox records already read, served as a feed (the real backend needs a reactor).
struct Records(Vec<OutboxRecord>);

impl OutboxFeed for Records {
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

#[test]
fn the_written_event_is_chained_once_by_the_lane_appender_over_the_postgres_state() {
    let Some(fx) = Fixture::new() else {
        return;
    };
    fx.exec(
        &APPENDER_TABLE_DDL
            .iter()
            .map(|(_, ddl)| *ddl)
            .collect::<Vec<_>>()
            .join(";"),
    );
    let writer = fx.writer("site-a");
    fx.rt
        .block_on(writer.write(&denial("site-a")))
        .expect("write");
    let feed = Records(fx.audits());
    assert_eq!(feed.0.len(), 1);

    let chain = Arc::new(InMemoryStore::new());
    let signer: Arc<dyn CheckpointSigner> = Arc::new(LocalSigner::from_seed([7u8; 32]));
    let state = Arc::new(PgAppenderStore::new(fx.pool(), "site-a"));
    let appender = LaneAppender::new(
        Arc::clone(&chain),
        "site-a",
        0,
        LaneConfig::new(1, 1),
        signer,
        Arc::clone(&state) as Arc<dyn spot_doc_audit::AppenderStore>,
    );
    let first = fx.rt.block_on(appender.drain(&feed, 100)).expect("drain");
    assert_eq!(first.appended, 1);
    // A second drain finds nothing new: the bookmark moved and the chain holds the event once.
    let second = fx
        .rt
        .block_on(appender.drain(&feed, 100))
        .expect("drain again");
    assert_eq!(second.appended, 0);
    let entries = fx
        .rt
        .block_on(chain.range(&lane_site("site-a", 0), 1, u64::MAX))
        .expect("range");
    assert_eq!(entries.len(), 1, "the denial is chained exactly once");
}
