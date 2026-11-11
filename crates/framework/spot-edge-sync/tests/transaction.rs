//! The post-write seam in one transaction: a failing edge write fails the document operation
//! that shares its transaction, and a successful one commits with the document.

// Test code may unwrap and expect what the test itself built.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{block_on, doc, name, rid, schema, set};
use spot_app_api::prelude::Value;
use spot_edge_sync::{EdgeSync, LifecycleHook, MemoryEdgeWriter};
use spot_storage_contract::{
    Collation, DocumentStore, Isolation, RowLock, StorageBackend, Transaction, TxOptions,
};
use spot_storage_fake::FakeBackend;

fn customer_order(id: u64, customer: u64) -> spot_app_api::Document {
    let mut order = doc("order", rid(id));
    set(&mut order, "customer", Value::Id(rid(customer)));
    order
}

fn read_options() -> TxOptions {
    TxOptions::new(name("edge_sync_read"), Isolation::ReadCommitted).read_only()
}

#[test]
fn a_failing_edge_write_rolls_back_the_document_write() {
    let meta = common::meta();
    let catalog = Arc::new(
        meta.catalog(Collation::new("und").expect("und is a collation"))
            .expect("the fixtures compile"),
    );
    let backend = FakeBackend::new();
    block_on(backend.prepare(&catalog)).expect("the store prepares");
    let schema = schema();
    let order = customer_order(1, 2);
    let stored =
        spot_document::mapping::to_storage(&meta, &order).expect("the order maps to storage");

    // The document write and the edge write share one transaction, as the engine's post-write
    // seam does. The edge write fails, so the transaction is dropped and rolls back.
    let mut tx = block_on(backend.begin(
        catalog.clone(),
        TxOptions::new(name("edge_sync_write"), Isolation::ReadCommitted),
    ))
    .expect("begin");
    block_on(tx.insert(&stored)).expect("the document insert succeeds before the edge write");
    let mut writer = common::FailingWriter::default();
    let result = block_on(EdgeSync::new().after_write(&schema, None, Some(&order), &mut writer));
    assert!(result.is_err(), "the failing edge write must surface");
    assert_eq!(writer.calls, 1, "the writer was asked to add the edge");
    drop(tx);

    // Neither the document nor its edge survives.
    let mut read = block_on(backend.begin(catalog.clone(), read_options())).expect("begin");
    let found = block_on(read.load(&name("order"), rid(1), RowLock::None)).expect("load");
    assert!(
        found.is_none(),
        "the document write rolled back with the failed edge write"
    );
}

#[test]
fn a_document_and_its_edges_commit_together() {
    let meta = common::meta();
    let catalog = Arc::new(
        meta.catalog(Collation::new("und").expect("und is a collation"))
            .expect("the fixtures compile"),
    );
    let backend = FakeBackend::new();
    block_on(backend.prepare(&catalog)).expect("the store prepares");
    let schema = schema();
    let order = customer_order(1, 2);
    let stored =
        spot_document::mapping::to_storage(&meta, &order).expect("the order maps to storage");
    let party = doc("party", rid(2));
    let stored_party =
        spot_document::mapping::to_storage(&meta, &party).expect("the party maps to storage");

    let mut writer = MemoryEdgeWriter::new();
    let mut tx = block_on(backend.begin(
        catalog.clone(),
        TxOptions::new(name("edge_sync_write"), Isolation::ReadCommitted),
    ))
    .expect("begin");
    block_on(tx.insert(&stored_party)).expect("insert the party");
    block_on(tx.insert(&stored)).expect("insert");
    block_on(EdgeSync::new().after_write(&schema, None, Some(&order), &mut writer))
        .expect("the edge write succeeds");
    block_on(tx.commit()).expect("commit");

    assert_eq!(writer.len(), 1, "the edge is written with the document");
    let mut read = block_on(backend.begin(catalog.clone(), read_options())).expect("begin");
    let found = block_on(read.load(&name("order"), rid(1), RowLock::None)).expect("load");
    assert!(found.is_some(), "the document committed");
}
