//! The control schema and the journal against a real database (skipped, with a `SKIPPED` line,
//! when `SPOT_TEST_PG_URL` is unset).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_migration_apply::journal::{ApplyJournal, JournalRecord, Progress};
use spot_migration_apply::{RestoreCause, RestoreMechanism, Version};
use spot_migration_apply_pg::{CONTROL_SCHEMA, PgApplyJournal, PgApplySchema, TABLES};
use support::*;

#[tokio::test]
async fn prepare_creates_the_control_schema_outside_the_tenant_tables_and_is_idempotent() {
    let Some(db) = db().await else { return };
    let pool = pool(&db, 4);
    let schema = PgApplySchema::new(pool.clone());
    schema.prepare().await.unwrap();
    schema.prepare().await.unwrap();

    let client = db.raw_client().await;
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = $1 ORDER BY table_name",
            &[&CONTROL_SCHEMA],
        )
        .await
        .unwrap();
    let mut names: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
    names.sort();
    let mut expected = TABLES.map(str::to_owned).to_vec();
    expected.sort();
    assert_eq!(names, expected);

    // The tenant schema holds none of them: a migration's diff can never see the journal.
    let public = client
        .query(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'",
            &[],
        )
        .await
        .unwrap();
    for name in &names {
        assert!(
            !public.iter().any(|row| row.get::<_, String>(0) == *name),
            "{name} leaked into public"
        );
    }
}

#[tokio::test]
async fn a_journal_round_trips_every_record_and_folds_like_the_in_memory_one() {
    let Some(db) = db().await else { return };
    let pool = pool(&db, 4);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    let journal = PgApplyJournal::new(pool, site());

    let records = [
        JournalRecord::Started {
            plan_digest: "plan-1".to_owned(),
            from_version: 1,
            to_version: 2,
            snapshot: "snap-1".to_owned(),
        },
        JournalRecord::GateClosing {
            reason: "migration".to_owned(),
        },
        JournalRecord::GateClosed,
        JournalRecord::StepStarted {
            index: 0,
            id: "schema/1".to_owned(),
            transactional: true,
            idempotency: Some(spot_migration_apply::Idempotency::Idempotent),
            restore_point: None,
        },
        JournalRecord::StepCompleted {
            index: 0,
            id: "schema/1".to_owned(),
            checkpoint: Some("ck-0".to_owned()),
        },
        JournalRecord::VerifyStarted,
        JournalRecord::Verified,
        JournalRecord::GateOpened {
            version: Version::New,
            verified: true,
        },
    ];
    for (offset, record) in records.iter().enumerate() {
        let seq = u64::try_from(offset).unwrap() + 1;
        journal
            .append(seq, record.clone(), 1_000 + i64::try_from(offset).unwrap())
            .await
            .unwrap();
    }

    let entries = journal.entries().await.unwrap();
    assert_eq!(entries.len(), records.len());
    for (entry, record) in entries.iter().zip(records.iter()) {
        assert_eq!(&entry.record, record);
    }
    let progress = Progress::fold(entries.clone());
    assert!(progress.started);
    assert_eq!(progress.plan_digest.as_deref(), Some("plan-1"));
    assert_eq!(progress.completed.len(), 1);
    assert!(progress.verified);
    assert_eq!(progress.gate_open, Some((Version::New, true)));
    assert!(!progress.in_progress());

    // A second journal over the same pool reads the same records, as a restart would.
    let restarted = PgApplyJournal::new(journal_pool(&db), site());
    assert_eq!(restarted.entries().await.unwrap(), entries);
}

fn journal_pool(db: &spot_postgres::testing::TestDb) -> deadpool_postgres::Pool {
    pool(db, 2)
}

#[tokio::test]
async fn a_sequence_that_is_not_the_next_one_is_refused() {
    let Some(db) = db().await else { return };
    let pool = pool(&db, 4);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    let journal = PgApplyJournal::new(pool, site());
    let started = JournalRecord::Started {
        plan_digest: "plan-1".to_owned(),
        from_version: 1,
        to_version: 2,
        snapshot: "snap-1".to_owned(),
    };
    journal.append(1, started.clone(), 1_000).await.unwrap();
    // Skipping 2 is refused, and so is repeating 1.
    assert!(journal.append(3, started.clone(), 1_001).await.is_err());
    assert!(journal.append(1, started.clone(), 1_002).await.is_err());
    assert_eq!(journal.next_seq().await.unwrap(), 2);
    // The refused writes left nothing behind.
    assert_eq!(journal.entries().await.unwrap().len(), 1);
    journal
        .append(
            2,
            JournalRecord::RestoreStarted {
                cause: RestoreCause::Recovery,
                mechanism: RestoreMechanism::Snapshot,
                restore_point: None,
            },
            1_003,
        )
        .await
        .unwrap();
    assert_eq!(journal.next_seq().await.unwrap(), 3);
}

#[tokio::test]
async fn two_sites_keep_independent_journals_in_one_database() {
    let Some(db) = db().await else { return };
    let pool = pool(&db, 4);
    PgApplySchema::new(pool.clone()).prepare().await.unwrap();
    let a = PgApplyJournal::new(pool.clone(), site());
    let b = PgApplyJournal::new(pool, spot_core::SiteId::new("other-site").unwrap());
    let record = JournalRecord::Started {
        plan_digest: "plan-a".to_owned(),
        from_version: 1,
        to_version: 2,
        snapshot: "snap-a".to_owned(),
    };
    a.append(1, record, 1_000).await.unwrap();
    assert_eq!(b.entries().await.unwrap().len(), 0);
    assert_eq!(b.next_seq().await.unwrap(), 1);
}
