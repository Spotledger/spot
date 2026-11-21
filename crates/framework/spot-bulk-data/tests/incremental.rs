//! Incremental extraction: coalescing, tombstones, expired watermarks and idempotent replay.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{DOCTYPE, World, build, name, principal};
use spot_bulk_data::{
    BackendChangeReader, BackendPageReader, BulkLimits, ExportSpec, IncrementalBatch,
    IncrementalSpec, Watermark,
};
use spot_storage_contract::{
    DocumentData, DocumentStore, FeedPosition, Isolation, RecordId, RowLock, StorageBackend,
    Transaction, TxOptions, Value,
};

async fn feed_end(world: &World) -> FeedPosition {
    use spot_storage_contract::ChangeFeed;
    let changes = world
        .backend
        .read_changes(FeedPosition::START, 10_000)
        .await
        .unwrap();
    changes
        .last()
        .map(|c| c.position)
        .unwrap_or(FeedPosition::START)
}

async fn insert(world: &World, id: RecordId, title: &str) {
    let mut tx = world
        .backend
        .begin(
            world.catalog.clone(),
            TxOptions::new(name("ins"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let mut fields = spot_storage_contract::Fields::new();
    fields.insert(name("modified"), Value::DateTime(world.base));
    fields.insert(name("title"), Value::Text(title.into()));
    fields.insert(name("owner"), Value::Text("analyst".into()));
    fields.insert(name("region"), Value::Text("north".into()));
    fields.insert(
        name("amount"),
        Value::Decimal(rust_decimal::Decimal::new(1, 2)),
    );
    fields.insert(name("note"), Value::Text("n".into()));
    fields.insert(name("card_number"), Value::Text("c".into()));
    fields.insert(name("salary"), Value::Text("s".into()));
    tx.insert(&DocumentData {
        entity: name(DOCTYPE),
        id,
        fields,
        children: std::collections::BTreeMap::new(),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn update_title(world: &World, id: RecordId, title: &str) {
    let mut tx = world
        .backend
        .begin(
            world.catalog.clone(),
            TxOptions::new(name("upd"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let doc = tx
        .load(&name(DOCTYPE), id, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    let mut data = doc.data;
    data.fields.insert(name("title"), Value::Text(title.into()));
    tx.update(&data, doc.version).await.unwrap();
    tx.commit().await.unwrap();
}

async fn delete(world: &World, id: RecordId) {
    let mut tx = world
        .backend
        .begin(
            world.catalog.clone(),
            TxOptions::new(name("del"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let doc = tx
        .load(&name(DOCTYPE), id, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    tx.delete(&name(DOCTYPE), id, doc.version).await.unwrap();
    tx.commit().await.unwrap();
}

async fn run(
    world: &World,
    since: Watermark,
) -> (Vec<Vec<Value>>, Vec<RecordId>, Option<Watermark>, u64) {
    let plan = Arc::new(world.plan(&ExportSpec::csv(DOCTYPE), &principal("analyst")));
    let reader = Arc::new(BackendPageReader::new(
        world.backend.clone(),
        world.catalog.clone(),
    ));
    let changes = BackendChangeReader::new(world.backend.clone());
    let spec = IncrementalSpec {
        doctype: DOCTYPE.into(),
        columns: Vec::new(),
        since,
    };
    let limits = BulkLimits {
        rows_per_chunk: 2,
        ..BulkLimits::default()
    };
    let mut batch = IncrementalBatch::extract(plan, &spec, reader, &changes, since.epoch, limits)
        .await
        .unwrap();
    let version = batch.version();
    assert!(
        batch.new_watermark().is_none(),
        "the watermark leaked early"
    );
    let mut upserts = Vec::new();
    let mut deletes = Vec::new();
    while let Some(chunk) = batch.next_chunk().await.unwrap() {
        upserts.extend(chunk.upserts);
        deletes.extend(chunk.deletes);
    }
    let watermark = batch.new_watermark();
    (upserts, deletes, watermark, version)
}

#[tokio::test]
async fn insert_update_twice_delete_gives_one_upsert_and_one_tombstone() {
    let world = build(6).await;
    let since = Watermark {
        position: feed_end(&world).await,
        epoch: 0,
    };
    let a = RecordId::new_v7();
    let c = world.docs[2].id;
    insert(&world, a, "A").await;
    update_title(&world, a, "A2").await;
    update_title(&world, a, "A3").await;
    delete(&world, c).await;

    let (upserts, deletes, watermark, version) = run(&world, since).await;
    assert_eq!(upserts.len(), 1, "one document changed: {upserts:?}");
    assert_eq!(deletes, vec![c], "the delete is a tombstone");
    // The upsert is the latest state of A.
    let plan = world.plan(&ExportSpec::csv(DOCTYPE), &principal("analyst"));
    let title_at = plan
        .columns
        .iter()
        .position(|col| col.path == "title")
        .unwrap();
    assert_eq!(upserts[0][title_at], Value::Text("A3".into()));
    assert!(
        watermark.is_some(),
        "the watermark is returned only after the last chunk"
    );
    assert!(version >= 3, "the batch carries the latest version");
}

#[tokio::test]
async fn a_watermark_from_another_epoch_is_expired() {
    let world = build(3).await;
    let since = Watermark {
        position: feed_end(&world).await,
        epoch: 7,
    };
    let plan = Arc::new(world.plan(&ExportSpec::csv(DOCTYPE), &principal("analyst")));
    let reader = Arc::new(BackendPageReader::new(
        world.backend.clone(),
        world.catalog.clone(),
    ));
    let changes = BackendChangeReader::new(world.backend.clone());
    let spec = IncrementalSpec {
        doctype: DOCTYPE.into(),
        columns: Vec::new(),
        since,
    };
    let err = IncrementalBatch::extract(plan, &spec, reader, &changes, 0, BulkLimits::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), spot_bulk_data::WATERMARK_EXPIRED);
}

#[tokio::test]
async fn a_watermark_from_another_generation_is_expired() {
    let world = build(3).await;
    let since = Watermark {
        position: FeedPosition::new(99, 1),
        epoch: 0,
    };
    let plan = Arc::new(world.plan(&ExportSpec::csv(DOCTYPE), &principal("analyst")));
    let reader = Arc::new(BackendPageReader::new(
        world.backend.clone(),
        world.catalog.clone(),
    ));
    let changes = BackendChangeReader::new(world.backend.clone());
    let spec = IncrementalSpec {
        doctype: DOCTYPE.into(),
        columns: Vec::new(),
        since,
    };
    let err = IncrementalBatch::extract(plan, &spec, reader, &changes, 0, BulkLimits::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), spot_bulk_data::WATERMARK_EXPIRED);
}

#[tokio::test]
async fn replaying_a_batch_is_idempotent() {
    let world = build(4).await;
    let since = Watermark {
        position: feed_end(&world).await,
        epoch: 0,
    };
    insert(&world, RecordId::new_v7(), "X").await;
    update_title(&world, world.docs[0].id, "changed").await;

    let first = run(&world, since).await;
    let second = run(&world, since).await;
    assert_eq!(first.0, second.0, "upserts differ on replay");
    assert_eq!(first.1, second.1, "deletes differ on replay");
    assert_eq!(first.3, second.3, "the version differs on replay");
    // The watermark advances past the changes.
    assert!(first.2.unwrap().position > since.position);
}

#[tokio::test]
async fn a_watermark_past_the_end_returns_no_changes() {
    let world = build(3).await;
    let end = feed_end(&world).await;
    let (upserts, deletes, watermark, _) = run(
        &world,
        Watermark {
            position: end,
            epoch: 0,
        },
    )
    .await;
    assert!(upserts.is_empty());
    assert!(deletes.is_empty());
    assert_eq!(watermark.unwrap().position, end);
}

#[tokio::test]
async fn extract_columns_must_match_the_plan() {
    let world = build(2).await;
    let plan = Arc::new(world.plan(&ExportSpec::csv(DOCTYPE), &principal("analyst")));
    let reader = Arc::new(BackendPageReader::new(
        world.backend.clone(),
        world.catalog.clone(),
    ));
    let changes = BackendChangeReader::new(world.backend.clone());
    let spec = IncrementalSpec {
        doctype: DOCTYPE.into(),
        columns: vec!["title".into()],
        since: Watermark {
            position: FeedPosition::START,
            epoch: 0,
        },
    };
    let err = IncrementalBatch::extract(plan, &spec, reader, &changes, 0, BulkLimits::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), spot_bulk_data::SPEC_INVALID);
}
