//! The image reader and `take_snapshot` against a real PostgreSQL (`SPOT_TEST_PG_URL`; skipped with
//! a message when unset): every record round-trips through the writer and the reader equal to the
//! database, a writer that commits during the snapshot does not appear, pages are bounded, and the
//! signed index is not written while the journal is behind.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use rust_decimal::Decimal;
use spot_journal_manifest::VerifyingKeys;
use spot_journal_sign::{Ed25519SnapshotSigner, ManifestSnapshotKeys};
use spot_journal_snapshot::IdentityTransform;
use spot_journal_snapshot::{SnapshotReader, list_snapshots};
use spot_journal_snapshot_pg::{
    ImageReader, JournalReach, SnapshotPgError, TakeSettings, take_snapshot,
};
use spot_journal_store::MemoryStore;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Catalog, ChangeFeed, ChildRow, DocumentData, DocumentStore, FeedPosition, Fields, Isolation,
    Name, RecordId, StorageBackend, Transaction, TxOptions, Value, contract_tests,
};

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

async fn env() -> Option<(TestDb, PgBackend, Arc<Catalog>)> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(contract_tests::catalog().unwrap());
    backend.prepare(&catalog).await.unwrap();
    Some((db, backend, catalog))
}

fn party(name: &str, cents: i64) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("name"), name.into());
    fields.insert(n("credit_limit"), Value::Decimal(Decimal::new(cents, 2)));
    for f in [
        "parent_party",
        "note",
        "since",
        "opens_at",
        "seen_at",
        "active",
        "visits",
        "profile",
        "logo",
    ] {
        fields.insert(n(f), Value::Null);
    }
    DocumentData {
        entity: n("party"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    }
}

fn invoice(number: &str, party: RecordId, lines: usize) -> DocumentData {
    let rows = (0..lines)
        .map(|i| {
            let mut fields = Fields::new();
            fields.insert(n("item"), format!("item-{i}").into());
            fields.insert(n("qty"), Value::Decimal(Decimal::new(1_000, 3)));
            fields.insert(n("amount"), Value::Decimal(Decimal::new(250, 2)));
            ChildRow {
                id: RecordId::new_v7(),
                fields,
            }
        })
        .collect();
    let mut fields = Fields::new();
    fields.insert(n("number"), number.into());
    fields.insert(n("party"), Value::Id(party));
    fields.insert(n("total"), Value::Decimal(Decimal::new(0, 2)));
    fields.insert(n("ref_entity"), Value::Null);
    fields.insert(n("ref_id"), Value::Null);
    let mut children = BTreeMap::new();
    children.insert(n("lines"), rows);
    DocumentData {
        entity: n("invoice"),
        id: RecordId::new_v7(),
        fields,
        children,
    }
}

async fn commit(backend: &PgBackend, catalog: &Arc<Catalog>, docs: &[DocumentData]) {
    let mut tx = backend
        .begin(
            Arc::clone(catalog),
            TxOptions::new(n("snapshot_seed"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    for doc in docs {
        tx.insert(doc).await.unwrap();
    }
    tx.commit().await.unwrap();
}

async fn publish(backend: &PgBackend) {
    let _ = backend.read_outbox(FeedPosition::START, 1).await;
}

struct Always(AtomicBool);

impl JournalReach for Always {
    fn reached<'a>(
        &'a self,
        _generation: u64,
        _seq: u64,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move { self.0.load(Ordering::SeqCst) })
    }
}

fn keys() -> (Ed25519SnapshotSigner, ManifestSnapshotKeys) {
    let signing = SigningKey::from_bytes(&[3u8; 32]);
    let mut verifying = VerifyingKeys::new();
    verifying
        .add_key("snapshot-key", signing.verifying_key(), 1, u64::MAX)
        .unwrap();
    (
        Ed25519SnapshotSigner::new("snapshot-key", signing),
        ManifestSnapshotKeys::new(verifying),
    )
}

fn settings() -> TakeSettings {
    TakeSettings {
        tenant: "acme".to_owned(),
        manifest_hash: "0".repeat(64),
        taken_at_millis: 1_700_000_000_000,
        page_size: 50,
        statement_timeout: Duration::from_secs(60),
        reach_timeout: Duration::from_millis(50),
        excluded: BTreeSet::new(),
    }
}

#[tokio::test]
async fn every_record_round_trips_and_a_concurrent_writer_is_invisible() {
    let Some((_db, backend, catalog)) = env().await else {
        return;
    };
    // 400 parties and 100 invoices of three lines each: several pages of each entity.
    let mut docs: Vec<DocumentData> = (0..400).map(|i| party(&format!("Acme {i}"), i)).collect();
    let anchor = docs[0].id;
    docs.extend((0..100).map(|i| invoice(&format!("INV-{i}"), anchor, 3)));
    for chunk in docs.chunks(100) {
        commit(&backend, &catalog, chunk).await;
    }
    publish(&backend).await;
    let mut reader = ImageReader::open(
        &backend,
        &catalog,
        &BTreeSet::new(),
        50,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    // A write that commits after the snapshot opened must not appear in it.
    commit(&backend, &catalog, &[party("Late arrival", 1)]).await;
    let mut seen = Vec::new();
    while let Some(page) = reader.next_page().await.unwrap() {
        assert!(page.len() <= 50, "bounded pages");
        seen.extend(page);
    }
    assert_eq!(
        seen.len(),
        500,
        "400 parties and 100 invoices, not the late one"
    );
    assert!(
        reader.pages_read() >= 10 && reader.pages_read() <= 12,
        "{}",
        reader.pages_read()
    );
    let invoice_image = seen.iter().find(|r| r.entity == "invoice").unwrap();
    assert_eq!(invoice_image.image["lines"].as_array().unwrap().len(), 3);
    assert_eq!(invoice_image.image["lines"][0]["idx"], 1);
    assert_eq!(invoice_image.image["id"], invoice_image.id);
    let sorted = {
        let mut keys: Vec<(String, String)> = seen
            .iter()
            .map(|r| (r.entity.clone(), r.id.clone()))
            .collect();
        let before = keys.clone();
        keys.sort();
        before == keys
    };
    assert!(sorted, "sorted by (entity, id)");
    reader.close().await;
}

#[tokio::test]
async fn the_snapshot_is_listed_only_when_the_journal_has_reached_its_position() {
    let Some((_db, backend, catalog)) = env().await else {
        return;
    };
    let docs: Vec<DocumentData> = (0..120).map(|i| party(&format!("Acme {i}"), i)).collect();
    commit(&backend, &catalog, &docs).await;
    publish(&backend).await;
    let store = MemoryStore::new();
    let (signer, snapshot_keys) = keys();
    let reach = Always(AtomicBool::new(false));
    // The journal is behind: parts may be written, but the signed index is not.
    let error = take_snapshot(
        &backend,
        &catalog,
        &store,
        &signer,
        &IdentityTransform,
        &reach,
        &settings(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, SnapshotPgError::JournalBehind);
    assert!(
        list_snapshots(&store, "acme", &snapshot_keys)
            .await
            .unwrap()
            .is_empty()
    );
    reach.0.store(true, Ordering::SeqCst);
    let info = take_snapshot(
        &backend,
        &catalog,
        &store,
        &signer,
        &IdentityTransform,
        &reach,
        &settings(),
    )
    .await
    .unwrap();
    let listed = list_snapshots(&store, "acme", &snapshot_keys)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].position, info.position);
    assert!(
        !listed[0].verified,
        "never verified until a rebuild test says so"
    );
    let reader = SnapshotReader::open(
        &store,
        "acme",
        info.generation,
        info.position,
        &snapshot_keys,
    )
    .await
    .unwrap();
    let mut stream = reader.images();
    let mut count = 0;
    while stream.next().await.unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 120);
}
