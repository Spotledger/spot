//! `PgEdgeWriters`, the `EdgeWriterFor<PgTransaction>` a host installs: the writer it makes
//! writes through the lent transaction, so the edge commits and rolls back with the document.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_app_api::prelude::SiteId;
use spot_edge_pg::PgEdgeWriters;
use spot_edge_sync::{EdgeChanges, EdgeWriterFor, apply_in};
use spot_postgres::PgTransaction;
use spot_storage_contract::{DocumentStore, NodeRef, Transaction};

/// Compile-time: the factory can be shared by the engine's participants.
const fn assert_send_sync<T: Send + Sync + Clone + Default + 'static>() {}
const _: () = assert_send_sync::<PgEdgeWriters>();

fn site() -> SiteId {
    SiteId::new("edges.pg").unwrap()
}

#[tokio::test]
async fn an_edge_written_through_the_factory_rolls_back_with_the_document() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    let edge = invoice_party_edge(inv.id, p.id);
    let factory = PgEdgeWriters;
    let site = site();

    let mut tx = env.begin("edge_factory_rollback").await;
    tx.insert(&p).await.unwrap();
    tx.insert(&inv).await.unwrap();
    {
        let mut writer = EdgeWriterFor::<PgTransaction>::writer(&factory, &mut tx, &site);
        let mut changes = EdgeChanges::default();
        changes.added.insert(edge.clone());
        apply_in(&mut *writer, &changes).await.unwrap();
        // The write is visible inside the transaction that holds the document.
        let owner = NodeRef {
            entity: n("invoice"),
            id: inv.id,
        };
        assert_eq!(writer.scan_by_source(&owner).await.unwrap(), vec![edge]);
    }
    // Outside the transaction nothing is visible yet: no second connection wrote it.
    assert_eq!(env.count("spot_graph_edge").await, 0);
    tx.rollback().await.unwrap();

    assert_eq!(env.count("spot_graph_edge").await, 0);
    assert!(load(&env, "invoice", inv.id).await.is_none());
}

#[tokio::test]
async fn an_edge_written_through_the_factory_commits_with_the_document() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    let edge = invoice_party_edge(inv.id, p.id);
    let factory = PgEdgeWriters;
    let site = site();

    let mut tx = env.begin("edge_factory_commit").await;
    tx.insert(&p).await.unwrap();
    tx.insert(&inv).await.unwrap();
    {
        let mut writer = EdgeWriterFor::<PgTransaction>::writer(&factory, &mut tx, &site);
        let mut changes = EdgeChanges::default();
        changes.added.insert(edge);
        apply_in(&mut *writer, &changes).await.unwrap();
    }
    tx.commit().await.unwrap();

    assert!(load(&env, "invoice", inv.id).await.is_some());
    assert_eq!(env.count("spot_graph_edge").await, 1);
}

#[tokio::test]
async fn a_failure_after_the_edge_write_removes_the_edge_with_the_document() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    let edge = invoice_party_edge(inv.id, p.id);
    let factory = PgEdgeWriters;
    let site = site();

    let mut tx = env.begin("edge_factory_fail").await;
    tx.insert(&p).await.unwrap();
    tx.insert(&inv).await.unwrap();
    {
        let mut writer = EdgeWriterFor::<PgTransaction>::writer(&factory, &mut tx, &site);
        let mut changes = EdgeChanges::default();
        changes.added.insert(edge);
        apply_in(&mut *writer, &changes).await.unwrap();
    }
    // A later step fails (a duplicate id), the engine drops the transaction.
    assert!(tx.insert(&p).await.is_err());
    drop(tx);

    assert_eq!(env.count("spot_graph_edge").await, 0);
    assert!(load(&env, "party", p.id).await.is_none());
}
