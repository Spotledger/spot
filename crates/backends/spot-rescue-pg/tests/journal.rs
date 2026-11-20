//! The PostgreSQL rescue journal against a real database (skipped with a printed `SKIPPED` line when
//! `SPOT_TEST_PG_URL` is unset): the contract of the in-memory journal, idempotent retry, gap and
//! fork refused, sites kept apart, concurrent appenders, concurrent `prepare`, every record variant
//! round-tripping, and no content in an error.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use spot_core::SiteId;
use spot_postgres::testing::TestDb;
use spot_rescue::{
    InvariantCheck, InvariantPhase, InvariantReport, ItemKey, ItemOutcome, JournalRecord,
    RescueJournal, RunIdentity, RunProgress,
};
use spot_rescue_pg::{CONTROL_SCHEMA, PgRescueJournal, PgRescueSchema, TABLES};

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

fn pool(db: &TestDb, max_size: usize) -> Pool {
    let config = db.config();
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&config.host)
        .port(config.port)
        .dbname(&config.dbname)
        .user(&config.user)
        .password(&config.password)
        .application_name("spot-rescue-pg-test");
    let manager = Manager::from_config(
        driver,
        tokio_postgres::NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(max_size)
        .runtime(Runtime::Tokio1)
        .build()
        .unwrap()
}

async fn journal(db: &TestDb, name: &str) -> PgRescueJournal {
    let pool = pool(db, 8);
    PgRescueSchema::new(pool.clone()).prepare().await.unwrap();
    PgRescueJournal::new(pool, site(name))
}

fn started(run: &str) -> JournalRecord {
    JournalRecord::Started {
        run_id: run.to_owned(),
        site: "s".to_owned(),
        identity: RunIdentity::new("platform:migration_rescue", "snap-1", 1).unwrap(),
        snapshot: "snap-1".to_owned(),
        snapshot_at: 1_000,
        attempt: 1,
        ticket_id: "ticket-1".to_owned(),
        approvers: vec!["alice".to_owned(), "bob".to_owned()],
    }
}

fn every_record() -> Vec<JournalRecord> {
    vec![
        started("run-1"),
        JournalRecord::Resumed { attempt: 2 },
        JournalRecord::GateClosing,
        JournalRecord::GateClosed,
        JournalRecord::RestoreStarted {
            snapshot: "snap-1".to_owned(),
        },
        JournalRecord::Restored {
            snapshot: "snap-1".to_owned(),
            location: "db-quarantine-1".to_owned(),
            at: 2_000,
        },
        JournalRecord::ItemOutcome {
            key: ItemKey::new("op-1"),
            outcome: ItemOutcome::Done,
        },
        JournalRecord::ItemOutcome {
            key: ItemKey::new("op-2"),
            outcome: ItemOutcome::skipped("reentry.skipped", "already there"),
        },
        JournalRecord::ItemOutcome {
            key: ItemKey::new("op-3"),
            outcome: ItemOutcome::failed("reentry.failed", "rejected"),
        },
        JournalRecord::Checkpoint {
            processed: 3,
            done: 1,
            skipped: 1,
            failed: 1,
            last_key: Some(ItemKey::new("op-3")),
        },
        JournalRecord::InvariantsChecked {
            report: InvariantReport::new(
                InvariantPhase::AfterReentry,
                vec![InvariantCheck::passed("trial_balance")],
            ),
        },
        JournalRecord::GateOpened,
        JournalRecord::Halted {
            code: "rescue.halted".to_owned(),
            reason: "needs a person".to_owned(),
        },
    ]
}

#[tokio::test]
async fn prepare_creates_the_control_schema_outside_the_tenant_tables_and_is_idempotent() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 4);
    let schema = PgRescueSchema::new(pool.clone());
    schema.prepare().await.unwrap();
    schema.prepare().await.unwrap();
    let client = db.raw_client().await;
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = $1",
            &[&CONTROL_SCHEMA],
        )
        .await
        .unwrap();
    let names: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
    assert_eq!(names, TABLES.map(str::to_owned).to_vec());
    // `spot-pg-migrate` reads `public` only: the journal is outside everything it can diff.
    let public = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'",
            &[],
        )
        .await
        .unwrap();
    assert!(
        !public
            .iter()
            .any(|row| row.get::<_, String>(0) == "spot_rescue_journal")
    );
}

#[tokio::test]
async fn prepare_called_concurrently_from_eight_tasks_succeeds() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 8);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let schema = PgRescueSchema::new(pool.clone());
        tasks.push(tokio::spawn(async move { schema.prepare().await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn every_record_variant_round_trips_in_order_and_folds_like_the_in_memory_journal() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let journal = journal(&db, "alpha").await;
    let records = every_record();
    for (index, record) in records.iter().enumerate() {
        let seq = u64::try_from(index).unwrap() + 1;
        journal
            .append(seq, record.clone(), 10 * i64::try_from(seq).unwrap())
            .await
            .unwrap();
    }
    let entries = journal.entries().await.unwrap();
    assert_eq!(entries.len(), records.len());
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(entry.seq, u64::try_from(index).unwrap() + 1);
        assert_eq!(entry.at, 10 * i64::try_from(entry.seq).unwrap());
        assert_eq!(entry.record, records[index]);
    }
    let progress = RunProgress::fold(entries);
    assert!(progress.started);
    assert_eq!(progress.run_id.as_deref(), Some("run-1"));
    assert_eq!(progress.last_seq, u64::try_from(records.len()).unwrap());
    assert!(progress.gate_open);
}

#[tokio::test]
async fn an_empty_journal_starts_at_one_and_a_gap_or_a_fork_is_refused_with_nothing_written() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let journal = journal(&db, "alpha").await;
    assert!(journal.entries().await.unwrap().is_empty());
    // Not 2 first.
    let error = journal.append(2, started("r"), 1).await.unwrap_err();
    assert!(
        error.to_string().contains("rescue_pg.journal_sequence"),
        "{error}"
    );
    journal.append(1, started("r"), 1).await.unwrap();
    journal
        .append(2, JournalRecord::GateClosing, 2)
        .await
        .unwrap();
    // A gap.
    assert!(
        journal
            .append(4, JournalRecord::GateClosed, 3)
            .await
            .is_err()
    );
    // A fork: a different record at an existing seq.
    assert!(
        journal
            .append(2, JournalRecord::GateClosed, 2)
            .await
            .is_err()
    );
    // A seq behind the head that is not the head.
    assert!(journal.append(1, started("r"), 1).await.is_err());
    assert_eq!(
        journal.entries().await.unwrap().len(),
        2,
        "nothing was written"
    );
}

#[tokio::test]
async fn the_same_record_at_the_same_seq_is_an_idempotent_retry() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let journal = journal(&db, "alpha").await;
    journal.append(1, started("r"), 1).await.unwrap();
    journal.append(1, started("r"), 1).await.unwrap();
    journal
        .append(2, JournalRecord::GateClosing, 2)
        .await
        .unwrap();
    journal
        .append(2, JournalRecord::GateClosing, 2)
        .await
        .unwrap();
    assert_eq!(journal.entries().await.unwrap().len(), 2);
}

#[tokio::test]
async fn two_sites_in_one_schema_stay_separate() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 4);
    PgRescueSchema::new(pool.clone()).prepare().await.unwrap();
    let alpha = PgRescueJournal::new(pool.clone(), site("alpha"));
    let beta = PgRescueJournal::new(pool, site("beta"));
    alpha.append(1, started("a"), 1).await.unwrap();
    beta.append(1, started("b"), 1).await.unwrap();
    beta.append(2, JournalRecord::GateClosing, 2).await.unwrap();
    assert_eq!(alpha.entries().await.unwrap().len(), 1);
    assert_eq!(beta.entries().await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_appenders_for_one_site_exactly_one_wins_each_seq() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 8);
    PgRescueSchema::new(pool.clone()).prepare().await.unwrap();
    let a = PgRescueJournal::new(pool.clone(), site("alpha"));
    let b = PgRescueJournal::new(pool, site("alpha"));
    a.append(1, started("r"), 1).await.unwrap();
    let (ra, rb) = tokio::join!(
        a.append(2, JournalRecord::GateClosing, 2),
        b.append(2, JournalRecord::GateClosed, 2)
    );
    assert!(
        ra.is_ok() != rb.is_ok(),
        "exactly one of two different records wins seq 2"
    );
    assert_eq!(a.entries().await.unwrap().len(), 2);
}

#[tokio::test]
async fn an_error_never_echoes_a_record_and_a_stored_record_that_does_not_parse_names_its_seq() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let journal = journal(&db, "alpha").await;
    let canary = "CANARY-in-a-detail";
    journal
        .append(
            1,
            JournalRecord::Halted {
                code: "c".to_owned(),
                reason: canary.to_owned(),
            },
            1,
        )
        .await
        .unwrap();
    let error = journal
        .append(
            5,
            JournalRecord::Halted {
                code: "c".to_owned(),
                reason: canary.to_owned(),
            },
            1,
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains(canary), "{error}");
    let client = db.raw_client().await;
    client
        .batch_execute(
            "INSERT INTO spot_rescue_control.spot_rescue_journal (site, seq, at, record) \
             VALUES ('alpha', 2, 1, '{\"record\": \"nonsense\"}')",
        )
        .await
        .unwrap();
    let error = journal.entries().await.unwrap_err().to_string();
    assert!(
        error.contains("rescue_pg.journal_record") && error.contains('2'),
        "{error}"
    );
    assert!(!error.contains("nonsense"));
}
