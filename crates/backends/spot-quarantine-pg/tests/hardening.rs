//! The hardening of the queue's database (WP-EDG-06.T7): the constraints are
//! validated, a role's right to change `state` can be checked, and an over-long sender name
//! becomes a marker on the real store without stalling the stream.
//!
//! Skips cleanly (a `SKIPPED:` line from `TestDb::create`) when `SPOT_TEST_PG_URL` is unset.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{catalog, ghost, note, options, pool, record_from, sender, store};
use spot_postgres::testing::TestDb;
use spot_quarantine::{
    AllowAll, FixedCatalog, Intake, IntakeOutcome, ItemFilter, MAX_SOURCE_BYTES, QuarantineStore,
    RecordingAudit,
};
use spot_quarantine_pg::{PgQuarantineStore, check_state_grant, check_state_grant_on};
use spot_replica_apply::{MirrorAll, RecordingAudit as ReplicaRecording};
use spot_storage_contract::RecordId;
use spot_storage_fake::FakeBackend;

const CONSTRAINTS: [&str; 3] = [
    "spot_quarantine_item_sizes",
    "spot_quarantine_item_closure",
    "spot_quarantine_payload_size",
];

async fn validated(db: &TestDb) -> Vec<(String, bool)> {
    db.raw_client()
        .await
        .query(
            "SELECT conname::text, convalidated FROM pg_constraint \
             WHERE conname LIKE 'spot\\_quarantine\\_%' AND contype = 'c' ORDER BY conname::text",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

#[tokio::test]
async fn prepare_validates_the_constraints_and_is_idempotent() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let _store = store(&db, "validate").await;
    let state = validated(&db).await;
    for name in CONSTRAINTS {
        assert!(
            state.iter().any(|(n, ok)| n == name && *ok),
            "{name} must be convalidated: {state:?}"
        );
    }
    // Preparing again changes nothing and still succeeds.
    PgQuarantineStore::new(pool(&db, 2, "again"))
        .prepare()
        .await
        .unwrap();
    assert_eq!(validated(&db).await, state);
}

#[tokio::test]
async fn a_not_valid_constraint_from_an_older_database_is_validated() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let _store = store(&db, "first").await;
    let raw = db.raw_client().await;
    raw.batch_execute(
        "ALTER TABLE spot_quarantine_item DROP CONSTRAINT spot_quarantine_item_sizes; \
         ALTER TABLE spot_quarantine_item ADD CONSTRAINT spot_quarantine_item_sizes CHECK (\
             octet_length(source) <= 128 AND octet_length(reason_code) <= 128 \
             AND octet_length(detail) <= 4096) NOT VALID;",
    )
    .await
    .unwrap();
    assert!(
        validated(&db)
            .await
            .iter()
            .any(|(n, ok)| n == "spot_quarantine_item_sizes" && !*ok),
        "the fixture left the constraint NOT VALID"
    );
    let _again = store(&db, "second").await;
    assert!(validated(&db).await.iter().all(|(_, ok)| *ok));
}

#[tokio::test]
async fn a_row_that_breaks_a_constraint_stops_prepare_without_leaking_it() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let _store = store(&db, "first").await;
    let raw = db.raw_client().await;
    raw.batch_execute(
        "ALTER TABLE spot_quarantine_item DROP CONSTRAINT spot_quarantine_item_sizes;",
    )
    .await
    .unwrap();
    let wide = "w".repeat(200);
    raw.execute(
        "INSERT INTO spot_quarantine_item (source, seq, record_name, record_version, entity, \
         record_id, reason_code, detail, first_seen, state) \
         VALUES ($1, 1, 'n', 1, 'e', 'i', 'r', 'd', 0, 'held')",
        &[&wide],
    )
    .await
    .unwrap();
    let error = PgQuarantineStore::new(pool(&db, 2, "second"))
        .prepare()
        .await
        .unwrap_err();
    assert_eq!(error.code, spot_quarantine_pg::MIGRATION_FAILED);
    assert!(
        !format!("{error:?}").contains(&wide),
        "the offending value is not in the error"
    );
}

#[tokio::test]
async fn a_role_without_update_on_state_gets_the_all_clear() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "grant").await;
    let owner = check_state_grant(&pool(&db, 1, "owner")).await.unwrap();
    assert!(owner.can_update_state, "the table owner can change state");
    assert!(!owner.is_all_clear());
    assert!(
        owner
            .warning()
            .unwrap()
            .contains("spot_quarantine_item.state")
    );
    let _ = &store;

    let role = format!(
        "quar_feed_{}",
        RecordId::new_v7().to_string().replace('-', "")
    );
    let raw = db.raw_client().await;
    raw.batch_execute(&format!(
        "CREATE ROLE {role} NOLOGIN; \
         GRANT SELECT, INSERT ON spot_quarantine_item, spot_quarantine_payload TO {role}; \
         GRANT UPDATE (attempts) ON spot_quarantine_item TO {role};"
    ))
    .await
    .unwrap();
    raw.batch_execute(&format!("SET ROLE {role}"))
        .await
        .unwrap();
    let feeder = check_state_grant_on(&raw).await.unwrap();
    assert_eq!(feeder.role, role);
    assert!(feeder.is_all_clear(), "{feeder:?}");
    assert!(feeder.warning().is_none());
    raw.batch_execute("RESET ROLE").await.unwrap();

    // A column-level grant on `state` itself is reported.
    raw.batch_execute(&format!(
        "GRANT UPDATE (state) ON spot_quarantine_item TO {role}; SET ROLE {role}"
    ))
    .await
    .unwrap();
    let wide = check_state_grant_on(&raw).await.unwrap();
    assert!(wide.can_update_state && wide.warning().is_some());
    raw.batch_execute("RESET ROLE").await.unwrap();
    raw.batch_execute(&format!(
        "REVOKE ALL ON spot_quarantine_item, spot_quarantine_payload FROM {role}; \
         DROP ROLE {role};"
    ))
    .await
    .unwrap();
}

#[tokio::test]
async fn an_overlong_sender_is_a_marker_on_the_real_store_and_the_stream_moves_on() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store: Arc<dyn QuarantineStore> = Arc::new(store(&db, "long").await);
    let intake = Intake::new(
        FakeBackend::new(),
        Arc::new(FixedCatalog::new(Arc::new(catalog(false)))),
        Arc::clone(&store),
        Arc::new(AllowAll),
        Arc::new(RecordingAudit::new()),
        Arc::new(MirrorAll),
        Arc::new(ReplicaRecording::new()),
        options(),
    );
    let label = "a".repeat(60);
    let from = sender(&format!("{label}.{label}.{label}.{label}"));
    assert!(from.as_str().len() > MAX_SOURCE_BYTES);

    let bad = record_from(&from, ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let marker = match intake.receive(&bad, 10).await.unwrap() {
        IntakeOutcome::TooLarge(item) => item,
        other => panic!("expected a marker, got {other:?}"),
    };
    assert!(marker.source.as_str().len() <= MAX_SOURCE_BYTES);
    assert!(!marker.detail.contains(&label));
    assert_eq!(intake.checkpoint(&from).unwrap().last_applied_seq, 1);
    assert_eq!(
        intake.receive(&bad, 11).await.unwrap(),
        IntakeOutcome::AlreadyQuarantined
    );
    let good = record_from(&from, note(RecordId::new_v7(), "ok", 1), 1, 2, vec![]);
    assert_eq!(
        intake.receive(&good, 12).await.unwrap(),
        IntakeOutcome::Applied
    );
    assert_eq!(store.list(&ItemFilter::all()).await.unwrap().len(), 1);
}
