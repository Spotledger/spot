//! Atomicity: a document and its edges commit or roll back together, because the writer runs in
//! the document's own transaction.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_edge_sync::{EdgeChanges, apply_in};
use spot_storage_contract::{DocumentStore, Transaction};

#[tokio::test]
async fn a_crash_after_the_document_insert_leaves_neither_the_document_nor_its_edges() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    let edge = invoice_party_edge(inv.id, p.id);

    {
        let mut tx = env.begin("edge_crash").await;
        tx.insert(&p).await.unwrap();
        tx.insert(&inv).await.unwrap();
        {
            let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
            let mut changes = EdgeChanges::default();
            changes.added.insert(edge.clone());
            apply_in(&mut writer, &changes).await.unwrap();
        }
        // The crash: the transaction is dropped without committing, so PostgreSQL rolls it back.
    }

    assert!(load(&env, "party", p.id).await.is_none());
    assert!(load(&env, "invoice", inv.id).await.is_none());
    assert_eq!(env.count("spot_graph_edge").await, 0);
}

#[tokio::test]
async fn a_committed_document_and_its_edges_are_both_visible() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    let edge = invoice_party_edge(inv.id, p.id);

    let mut tx = env.begin("edge_commit").await;
    tx.insert(&p).await.unwrap();
    tx.insert(&inv).await.unwrap();
    {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        let mut changes = EdgeChanges::default();
        changes.added.insert(edge);
        apply_in(&mut writer, &changes).await.unwrap();
    }
    tx.commit().await.unwrap();

    assert!(load(&env, "party", p.id).await.is_some());
    assert!(load(&env, "invoice", inv.id).await.is_some());
    assert_eq!(env.count("spot_graph_edge").await, 1);
}

#[tokio::test]
async fn a_failing_edge_write_rolls_the_document_write_back() {
    let Some(env) = env().await else {
        return;
    };
    // Remove the edge table: the edge write must fail, and the document with it.
    let client = env.db.raw_client().await;
    client
        .batch_execute("DROP TABLE spot_graph_edge")
        .await
        .unwrap();

    let p = party(&env);
    let inv = invoice(&env, p.id);
    let edge = invoice_party_edge(inv.id, p.id);

    let mut tx = env.begin("edge_fail").await;
    tx.insert(&p).await.unwrap();
    tx.insert(&inv).await.unwrap();
    let failed = {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        let mut changes = EdgeChanges::default();
        changes.added.insert(edge);
        apply_in(&mut writer, &changes).await.is_err()
    };
    assert!(failed, "the edge write must fail when the table is gone");
    tx.rollback().await.unwrap();

    assert!(load(&env, "invoice", inv.id).await.is_none());
}
