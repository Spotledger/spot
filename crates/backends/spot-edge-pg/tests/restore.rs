//! Restore: after restoring an earlier snapshot of the edge store, the verify job reports exactly
//! the drift that snapshot held, and the repair fixes it.
//!
//! The snapshot here is a logical copy of the tenant's `spot_graph_edge` table taken inside the same
//! database (`CREATE TABLE ... AS TABLE spot_graph_edge`), which is what a physical restore returns
//! the table to. `spot-snapshot`'s file-level providers need `pg_dump`/`pg_restore` (or a
//! stopped supervised instance), so they are not used here; see the crate's `## Open questions`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_edge_pg::{DriftKind, EdgeRepair, EdgeVerifyJob};
use spot_edge_sync::{EdgeChanges, apply_in};
use spot_storage_contract::{RecordId, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

fn uuid(id: RecordId) -> Uuid {
    Uuid::from_bytes(*id.as_bytes())
}

fn instant() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

async fn verify(env: &Env, key: &str) -> spot_edge_pg::DriftReport {
    EdgeVerifyJob::new(env.backend.clone(), env.catalog.clone(), env.schema())
        .run(instant(), key)
        .await
        .unwrap()
}

#[tokio::test]
async fn restore_from_a_snapshot_reports_exactly_the_drift_it_held() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    seed(&env, &[&p, &inv]).await;
    // A consistent starting point: the document's edge is stored.
    let mut tx = env.begin("edge_seed").await;
    {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        let mut changes = EdgeChanges::default();
        changes.added.insert(invoice_party_edge(inv.id, p.id));
        apply_in(&mut writer, &changes).await.unwrap();
    }
    tx.commit().await.unwrap();

    // Inject the drift that the snapshot will preserve: a missing edge and an orphan.
    let client = env.db.raw_client().await;
    client
        .execute(
            "DELETE FROM spot_graph_edge WHERE owner_id = $1",
            &[&uuid(inv.id)],
        )
        .await
        .unwrap();
    let orphan = RecordId::new_v7();
    client
        .execute(
            "INSERT INTO spot_graph_edge (edge_type, owner_entity, owner_id, from_entity, from_id, \
             to_entity, to_id, properties) VALUES ($1, 'invoice', $2, 'invoice', $2, 'party', \
             $3, '{}'::jsonb)",
            &[
                &spot_graph_model::EdgeTypeId::of("invoice.party")
                    .as_str()
                    .to_owned(),
                &uuid(orphan),
                &uuid(p.id),
            ],
        )
        .await
        .unwrap();

    let drifted = verify(&env, "before-snapshot").await;
    assert_eq!(drifted.entries.len(), 2, "{:?}", drifted.entries);
    assert!(drifted.entries.iter().any(|e| e.kind == DriftKind::Missing));
    assert!(drifted.entries.iter().any(|e| e.kind == DriftKind::Extra));

    // Take the snapshot of the drifted store, then repair the live store so it is clean.
    client
        .batch_execute("CREATE TABLE spot_graph_edge_snapshot AS TABLE spot_graph_edge")
        .await
        .unwrap();
    EdgeRepair::new(env.backend.clone(), env.catalog.clone())
        .repair(instant(), "before-snapshot")
        .await
        .unwrap();
    assert!(verify(&env, "repaired").await.is_clean());

    // Restore the snapshot: the drift is back.
    client
        .batch_execute(
            "TRUNCATE spot_graph_edge; \
             INSERT INTO spot_graph_edge SELECT * FROM spot_graph_edge_snapshot",
        )
        .await
        .unwrap();

    let restored = verify(&env, "after-restore").await;
    assert_eq!(
        restored.entries, drifted.entries,
        "the restore must report exactly the drift the snapshot held"
    );

    // And a repair of the restored state makes it clean again.
    EdgeRepair::new(env.backend.clone(), env.catalog.clone())
        .repair(instant(), "after-restore")
        .await
        .unwrap();
    assert!(verify(&env, "clean-again").await.is_clean());
}
