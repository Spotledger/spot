//! `PgEdgeWriter` on a real database: add, scan by source, update and remove, all in the
//! caller's transaction.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_edge_sync::{Edge, EdgeChanges, EdgeWriter, PropValue, apply_in};
use spot_storage_contract::{DocumentStore, NodeRef, Transaction};

fn owner(id: spot_storage_contract::RecordId) -> NodeRef {
    NodeRef {
        entity: n("invoice"),
        id,
    }
}

#[tokio::test]
async fn a_change_commits_with_the_document_it_shares_a_transaction_with() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    let edge = invoice_party_edge(inv.id, p.id);

    let mut tx = env.begin("edge_writer").await;
    tx.insert(&p).await.unwrap();
    tx.insert(&inv).await.unwrap();
    {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        let mut changes = EdgeChanges::default();
        changes.added.insert(edge.clone());
        apply_in(&mut writer, &changes).await.unwrap();
    }
    tx.commit().await.unwrap();

    // The edge is stored, and the bounded read returns exactly it.
    let mut tx = env.begin("edge_scan").await;
    let found = {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        writer.scan_by_source(&owner(inv.id)).await.unwrap()
    };
    tx.rollback().await.unwrap();
    assert_eq!(found, vec![edge]);
    assert_eq!(env.count("spot_graph_edge").await, 1);
}

#[tokio::test]
async fn an_edge_is_written_whole_so_an_update_replaces_its_properties() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    seed(&env, &[&p, &inv]).await;

    let mut edge = invoice_party_edge(inv.id, p.id);
    edge.properties
        .insert("weight".to_owned(), PropValue::Int(1));

    let mut tx = env.begin("edge_update").await;
    {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        let mut changes = EdgeChanges::default();
        changes.added.insert(edge.clone());
        apply_in(&mut writer, &changes).await.unwrap();
    }
    // The same key, new properties: an update, not a second row.
    let mut changed = edge.clone();
    changed
        .properties
        .insert("weight".to_owned(), PropValue::Int(2));
    {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        let mut changes = EdgeChanges::default();
        changes.updated.push(spot_edge_sync::EdgeUpdate {
            key: edge.key(),
            before: edge.clone(),
            after: changed.clone(),
        });
        apply_in(&mut writer, &changes).await.unwrap();
    }
    let found = {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        writer.scan_by_source(&owner(inv.id)).await.unwrap()
    };
    tx.commit().await.unwrap();

    assert_eq!(found, vec![changed]);
    assert_eq!(env.count("spot_graph_edge").await, 1);
}

#[tokio::test]
async fn a_remove_leaves_no_edge_for_the_source() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let inv = invoice(&env, p.id);
    seed(&env, &[&p, &inv]).await;
    let edge = invoice_party_edge(inv.id, p.id);

    let mut tx = env.begin("edge_remove").await;
    {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        let mut changes = EdgeChanges::default();
        changes.added.insert(edge.clone());
        apply_in(&mut writer, &changes).await.unwrap();
    }
    {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        let mut changes = EdgeChanges::default();
        changes.removed.insert(edge.clone());
        apply_in(&mut writer, &changes).await.unwrap();
    }
    let found = {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        writer.scan_by_source(&owner(inv.id)).await.unwrap()
    };
    tx.commit().await.unwrap();

    assert!(found.is_empty());
    assert_eq!(env.count("spot_graph_edge").await, 0);
}

#[tokio::test]
async fn scan_by_source_is_bounded_to_the_one_source() {
    let Some(env) = env().await else {
        return;
    };
    let p = party(&env);
    let one = invoice(&env, p.id);
    let two = invoice(&env, p.id);
    seed(&env, &[&p, &one, &two]).await;
    let first: Edge = invoice_party_edge(one.id, p.id);
    let second: Edge = invoice_party_edge(two.id, p.id);

    let mut tx = env.begin("edge_bounded").await;
    {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        let mut changes = EdgeChanges::default();
        changes.added.insert(first.clone());
        changes.added.insert(second.clone());
        apply_in(&mut writer, &changes).await.unwrap();
    }
    let found = {
        let mut writer = spot_edge_pg::PgEdgeWriter::new(&mut tx);
        writer.scan_by_source(&owner(one.id)).await.unwrap()
    };
    tx.commit().await.unwrap();

    assert_eq!(found, vec![first]);
    assert_eq!(env.count("spot_graph_edge").await, 2);
}
