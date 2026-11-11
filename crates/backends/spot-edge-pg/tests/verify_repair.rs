//! The `edge_verify` job and the `edge_repair` operation on a real database: drift is reported
//! exactly, repair fixes exactly what was reported, and nothing else is touched.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_edge_pg::{DriftKind, EdgeRepair, EdgeVerifyJob, read_report};
use spot_edge_sync::{Edge, EdgeChanges, EdgeWriter, apply_in};
use spot_storage_contract::{NodeRef, RecordId, StorageBackend, Transaction};
use std::sync::Arc;
use time::OffsetDateTime;
use uuid::Uuid;

fn instant(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

async fn store_edge(env: &Env, edge: &Edge) {
    let mut tx = env.begin("edge_seed_edge").await;
    {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        let mut changes = EdgeChanges::default();
        changes.added.insert(edge.clone());
        apply_in(&mut writer, &changes).await.unwrap();
    }
    tx.commit().await.unwrap();
}

fn uuid(id: RecordId) -> Uuid {
    Uuid::from_bytes(*id.as_bytes())
}

async fn verify(env: &Env, key: &str) -> spot_edge_pg::DriftReport {
    let job = EdgeVerifyJob::new(env.backend.clone(), env.catalog.clone(), env.schema());
    job.run(instant(1_700_000_000), key).await.unwrap()
}

async fn repair(env: &Env, key: &str) -> spot_edge_pg::RepairReport {
    let operation = EdgeRepair::new(env.backend.clone(), env.catalog.clone()).with_batch_size(1);
    operation.repair(instant(1_700_000_100), key).await.unwrap()
}

#[tokio::test]
async fn a_clean_tenant_reports_nothing() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    seed(&env, &[&p, &inv]).await;
    store_edge(&env, &invoice_party_edge(inv.id, p.id)).await;

    let report = verify(&env, "clean").await;
    assert!(report.is_clean(), "{:?}", report.entries);
    assert!(report.checked_documents >= 2);
}

#[tokio::test]
async fn verify_reports_missing_and_orphan_edges_and_repair_cleans_them() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    let other = invoice(&env, p.id);
    seed(&env, &[&p, &inv, &other]).await;
    store_edge(&env, &invoice_party_edge(inv.id, p.id)).await;
    store_edge(&env, &invoice_party_edge(other.id, p.id)).await;

    // Inject drift: delete the first invoice's edge (missing) and add an edge owned by a
    // document that does not exist (extra/orphan).
    let client = env.db.raw_client().await;
    client
        .execute(
            "DELETE FROM spot_graph_edge WHERE owner_id = $1",
            &[&uuid(inv.id)],
        )
        .await
        .unwrap();
    let orphan_owner = RecordId::new_v7();
    client
        .execute(
            "INSERT INTO spot_graph_edge (edge_type, owner_entity, owner_id, from_entity, from_id, \
             to_entity, to_id, properties) VALUES ($1, 'invoice', $2, 'invoice', $2, 'party', \
             $3, '{}'::jsonb)",
            &[
                &spot_graph_model::EdgeTypeId::of("invoice.party")
                    .as_str()
                    .to_owned(),
                &uuid(orphan_owner),
                &uuid(p.id),
            ],
        )
        .await
        .unwrap();

    let report = verify(&env, "drifted").await;
    assert_eq!(report.entries.len(), 2, "{:?}", report.entries);
    assert!(report.entries.iter().any(|e| e.kind == DriftKind::Missing));
    assert!(report.entries.iter().any(|e| e.kind == DriftKind::Extra));

    // Repair fixes exactly what the report named.
    let fixed = repair(&env, "drifted").await;
    assert_eq!(fixed.added, 1);
    assert_eq!(fixed.removed, 1);
    assert_eq!(fixed.updated, 0);
    // One change per batch, so two batches and two audit rows.
    assert_eq!(fixed.batches.len(), 2);
    assert_eq!(env.count("spot_edge_repair_audit").await, 2);

    // The unrelated invoice's edge was never touched, and a second verify is clean.
    let mut tx = env.begin("edge_check").await;
    let untouched = {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        writer
            .scan_by_source(&NodeRef {
                entity: n("invoice"),
                id: other.id,
            })
            .await
            .unwrap()
    };
    tx.rollback().await.unwrap();
    assert_eq!(untouched, vec![invoice_party_edge(other.id, p.id)]);

    let after = verify(&env, "after").await;
    assert!(after.is_clean(), "{:?}", after.entries);
}

#[tokio::test]
async fn verify_reports_a_property_mismatch_and_repair_updates_it() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    seed(&env, &[&p, &inv]).await;
    store_edge(&env, &invoice_party_edge(inv.id, p.id)).await;

    // Inject a property the document does not imply: a mismatch.
    let client = env.db.raw_client().await;
    client
        .execute(
            "UPDATE spot_graph_edge SET properties = '{\"weight\":{\"t\":\"int\",\"v\":9}}'::jsonb \
             WHERE owner_id = $1",
            &[&uuid(inv.id)],
        )
        .await
        .unwrap();

    let report = verify(&env, "mismatch").await;
    assert_eq!(report.entries.len(), 1, "{:?}", report.entries);
    assert_eq!(report.entries[0].kind, DriftKind::Mismatch);

    let fixed = repair(&env, "mismatch").await;
    assert_eq!(fixed.updated, 1);
    assert_eq!(fixed.added, 0);
    assert_eq!(fixed.removed, 0);

    let after = verify(&env, "after-mismatch").await;
    assert!(after.is_clean(), "{:?}", after.entries);
}

#[tokio::test]
async fn a_report_survives_a_round_trip_and_a_missing_report_is_a_named_error() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    seed(&env, &[&p, &inv]).await;
    store_edge(&env, &invoice_party_edge(inv.id, p.id)).await;
    let client = env.db.raw_client().await;
    client
        .execute("DELETE FROM spot_graph_edge", &[])
        .await
        .unwrap();

    let report = verify(&env, "round-trip").await;
    let mut tx = env.begin("edge_read").await;
    let read = read_report(&mut tx, "round-trip").await.unwrap();
    let missing = read_report(&mut tx, "no-such-report").await;
    tx.rollback().await.unwrap();
    assert_eq!(read, report);
    assert_eq!(missing.unwrap_err().code(), "edge_pg.report_not_found");

    let _ = env.backend.health().await;
}

#[tokio::test]
async fn the_verify_handler_writes_a_report_and_succeeds() {
    use spot_jobs::model::NewJob;
    use spot_jobs::store::{InMemoryJobStore, JobStore};
    use spot_jobs::worker::{HandlerResult, JobContext};

    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    seed(&env, &[&p, &inv]).await;
    store_edge(&env, &invoice_party_edge(inv.id, p.id)).await;

    let now = instant(1_700_000_000);
    let store = InMemoryJobStore::new();
    let site = spot_core::SiteId::new("edge-test").unwrap();
    let job = NewJob::new(
        "edge_verify".to_owned(),
        serde_json::json!({ "report_key": "handler" }),
        "default".to_owned(),
        site,
        now,
    );
    store.insert_if_new(job).await.unwrap();
    let claimed = store
        .claim(
            &["default".to_owned()],
            "worker",
            std::time::Duration::from_secs(30),
            now,
            1,
        )
        .await
        .unwrap();

    let verify_job = EdgeVerifyJob::new(env.backend.clone(), env.catalog.clone(), env.schema());
    let ctx = JobContext {
        job: claimed[0].job.clone(),
        attempt: 1,
        clock: now,
    };
    let result = spot_edge_pg::handler(Arc::new(verify_job))(ctx)
        .await
        .unwrap();
    assert_eq!(result, HandlerResult::Ok);

    let mut tx = env.begin("edge_read").await;
    let report = read_report(&mut tx, "handler").await.unwrap();
    tx.rollback().await.unwrap();
    assert!(report.is_clean());
}

#[tokio::test]
async fn the_verify_handler_refuses_a_job_without_a_report_key() {
    use spot_jobs::model::NewJob;
    use spot_jobs::store::{InMemoryJobStore, JobStore};
    use spot_jobs::worker::{HandlerResult, JobContext};

    let Some(env) = env().await else {
        return;
    };
    let now = instant(1_700_000_000);
    let store = InMemoryJobStore::new();
    let site = spot_core::SiteId::new("edge-test").unwrap();
    let job = NewJob::new(
        "edge_verify".to_owned(),
        serde_json::json!({}),
        "default".to_owned(),
        site,
        now,
    );
    store.insert_if_new(job).await.unwrap();
    let claimed = store
        .claim(
            &["default".to_owned()],
            "worker",
            std::time::Duration::from_secs(30),
            now,
            1,
        )
        .await
        .unwrap();

    let verify_job = EdgeVerifyJob::new(env.backend.clone(), env.catalog.clone(), env.schema());
    let ctx = JobContext {
        job: claimed[0].job.clone(),
        attempt: 1,
        clock: now,
    };
    let result = spot_edge_pg::handler(Arc::new(verify_job))(ctx)
        .await
        .unwrap();
    assert!(matches!(result, HandlerResult::Fail(_)));
}
