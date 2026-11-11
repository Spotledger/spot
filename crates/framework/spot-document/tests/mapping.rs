//! `mapping`: the Document <-> storage mapping, including round trips through the fake backend.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_app_api::prelude::{RecordId, Value, Version};
use spot_app_api::{DocState, Document};
use spot_document::mapping::{from_storage, parse_state, state_text, to_storage};
use spot_document::{coerce, defaults, validate};
use spot_storage_contract::{
    Catalog, Collation, DocumentStore, Isolation, StorageBackend, Transaction, TxOptions,
};
use spot_storage_fake::FakeBackend;

async fn backend() -> (FakeBackend, Arc<Catalog>) {
    let catalog = Arc::new(meta().catalog(Collation::new("en").unwrap()).unwrap());
    let backend = FakeBackend::new();
    backend.prepare(&catalog).await.unwrap();
    (backend, catalog)
}

fn options() -> TxOptions {
    TxOptions::new(n("mapping_test"), Isolation::ReadCommitted)
}

/// The customer an invoice links to, written first.
async fn insert_customer(backend: &FakeBackend, catalog: &Arc<Catalog>, id: RecordId) {
    let meta = meta();
    let mut customer = Document::new(n("customer"), id);
    customer.set(n("name"), "Acme");
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    tx.insert(&to_storage(&meta, &customer).unwrap())
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// Every declared field's value, unset ones as `Null`, and the tables. (The state is checked
/// separately: a new document has none until the engine gives it one.)
fn same_content(a: &Document, b: &Document) {
    let meta = meta();
    let def = meta.definition(a.doctype().as_str()).unwrap();
    for f in def.fields.iter().filter(|f| f.name != "doc_state") {
        assert_eq!(a.get(&f.name), b.get(&f.name), "field {}", f.name);
    }
    assert_eq!(a.id(), b.id());
    assert_eq!(a.rows("items").len(), b.rows("items").len());
    let child = meta.definition("sales_invoice_item").unwrap();
    for (x, y) in a.rows("items").iter().zip(b.rows("items")) {
        assert_eq!(x.id, y.id);
        for f in &child.fields {
            let get = |r: &spot_app_api::document::ChildRow| {
                r.fields.get(&n(&f.name)).cloned().unwrap_or(Value::Null)
            };
            assert_eq!(get(x), get(y), "row field {}", f.name);
        }
    }
}

#[test]
fn to_storage_is_a_full_write_with_nulls_and_ordered_rows() {
    let meta = meta();
    let mut doc = valid_invoice();
    doc.set(n("total"), money(350));
    let data = to_storage(&meta, &doc).unwrap();
    assert_eq!(data.entity, n("sales_invoice"));
    assert_eq!(data.id, doc.id());
    let names: Vec<&str> = data.fields.keys().map(|k| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "amended_from",
            "attachment",
            "created_at",
            "customer",
            "discount_days",
            "doc_state",
            "extra",
            "number",
            "posting_date",
            "remarks",
            "status",
            "total",
            "urgent"
        ]
    );
    assert_eq!(
        data.fields[&n("remarks")],
        Value::Null,
        "unset is written as null"
    );
    assert_eq!(data.fields[&n("total")], money(350));
    let rows = &data.children[&n("items")];
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, doc.rows("items")[0].id);
    assert_eq!(rows[1].id, doc.rows("items")[1].id);
    let row_fields: Vec<&str> = rows[0].fields.keys().map(|k| k.as_str()).collect();
    assert_eq!(row_fields, ["item", "kind", "qty", "rate"]);
    assert_eq!(rows[0].fields[&n("kind")], Value::Null);
}

#[test]
fn a_document_without_rows_still_names_its_tables() {
    let meta = meta();
    let mut doc = invoice();
    doc.set(n("number"), "SI-1");
    let data = to_storage(&meta, &doc).unwrap();
    assert_eq!(
        data.children.keys().map(|k| k.as_str()).collect::<Vec<_>>(),
        ["items"]
    );
    assert!(data.children[&n("items")].is_empty());
}

#[test]
fn the_state_comes_from_the_document_never_from_a_field() {
    let meta = meta();
    let mut doc = valid_invoice();
    // A new posted document has no state yet: it is a draft.
    assert_eq!(
        to_storage(&meta, &doc).unwrap().fields[&n("doc_state")],
        Value::Text("draft".into())
    );
    doc.mark_saved(Version(3), Some(DocState::Posted));
    assert_eq!(
        to_storage(&meta, &doc).unwrap().fields[&n("doc_state")],
        Value::Text("posted".into())
    );
    // A caller-set field cannot reach storage through the mapping, and validation refuses it.
    doc.set(n("doc_state"), "cancelled");
    assert_eq!(
        to_storage(&meta, &doc).unwrap().fields[&n("doc_state")],
        Value::Text("posted".into())
    );
    assert_eq!(
        validate::check(&meta, &doc).unwrap_err().fields[0].code,
        "document.read_only_field"
    );
}

#[test]
fn a_document_that_is_not_posted_has_no_state_field() {
    let meta = meta();
    let mut customer = Document::new(n("customer"), RecordId::new_v7());
    customer.set(n("name"), "Acme");
    let data = to_storage(&meta, &customer).unwrap();
    assert!(!data.fields.contains_key(&n("doc_state")));
    assert!(data.children.is_empty());
}

#[test]
fn the_state_names_round_trip() {
    for s in [DocState::Draft, DocState::Posted, DocState::Cancelled] {
        assert_eq!(parse_state(state_text(s)), Some(s));
    }
    assert_eq!(parse_state("Posted"), None);
    assert_eq!(parse_state(""), None);
}

#[tokio::test]
async fn insert_then_load_gives_the_same_document_with_nothing_changed() {
    let (backend, catalog) = backend().await;
    let meta = meta();
    let customer = RecordId::new_v7();
    insert_customer(&backend, &catalog, customer).await;

    let mut doc = valid_invoice();
    doc.set(n("customer"), customer);
    doc.set(n("status"), "Open");
    doc.set(n("total"), money(350));
    doc.set(n("remarks"), "");
    doc.set(n("attachment"), Value::Bytes(vec![0, 255, 7]));
    doc.rows_mut(n("items"))[0]
        .fields
        .insert(n("kind"), "Goods".into());
    defaults::apply(&meta, &mut doc, now()).unwrap();
    coerce::normalise(&meta, &mut doc).unwrap();
    validate::check(&meta, &doc).unwrap();

    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let version = tx.insert(&to_storage(&meta, &doc).unwrap()).await.unwrap();
    assert_eq!(version, Version::FIRST);
    tx.commit().await.unwrap();

    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let stored = tx
        .load(
            &n("sales_invoice"),
            doc.id(),
            spot_storage_contract::RowLock::None,
        )
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    let loaded = from_storage(&meta, stored).unwrap();

    same_content(&doc, &loaded);
    assert_eq!(loaded.version(), Some(Version::FIRST));
    assert_eq!(loaded.state(), Some(DocState::Draft));
    assert!(!loaded.is_new());
    assert!(
        loaded.changed_fields().is_empty(),
        "{:?}",
        loaded.changed_fields()
    );
    assert!(
        !loaded.fields().contains_key(&n("doc_state")),
        "the state is not a field"
    );
    // What came back validates, and writes back unchanged.
    validate::check(&meta, &loaded).unwrap();
    assert_eq!(
        to_storage(&meta, &loaded).unwrap().children,
        to_storage(&meta, &doc).unwrap().children
    );
}

#[tokio::test]
async fn editing_a_loaded_document_is_tracked_and_can_be_written_back() {
    let (backend, catalog) = backend().await;
    let meta = meta();
    let customer = RecordId::new_v7();
    insert_customer(&backend, &catalog, customer).await;
    let mut doc = valid_invoice();
    doc.set(n("customer"), customer);
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    tx.insert(&to_storage(&meta, &doc).unwrap()).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let stored = tx
        .load(
            &n("sales_invoice"),
            doc.id(),
            spot_storage_contract::RowLock::None,
        )
        .await
        .unwrap()
        .unwrap();
    let mut loaded = from_storage(&meta, stored).unwrap();
    loaded.set(n("total"), money(1_000));
    loaded.rows_mut(n("items")).remove(0);
    assert_eq!(
        loaded
            .changed_fields()
            .iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>(),
        ["items", "total"]
    );
    let version = tx
        .update(
            &to_storage(&meta, &loaded).unwrap(),
            loaded.version().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(version, Version(2));
    tx.commit().await.unwrap();

    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let again = from_storage(
        &meta,
        tx.load(
            &n("sales_invoice"),
            doc.id(),
            spot_storage_contract::RowLock::None,
        )
        .await
        .unwrap()
        .unwrap(),
    )
    .unwrap();
    assert_eq!(again.rows("items").len(), 1);
    assert_eq!(again.get("total"), &money(1_000));
}

#[tokio::test]
async fn a_posted_state_round_trips_through_storage() {
    let (backend, catalog) = backend().await;
    let meta = meta();
    let customer = RecordId::new_v7();
    insert_customer(&backend, &catalog, customer).await;
    let mut doc = valid_invoice();
    doc.set(n("customer"), customer);
    doc.mark_saved(Version::FIRST, Some(DocState::Cancelled));
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    tx.insert(&to_storage(&meta, &doc).unwrap()).await.unwrap();
    let stored = tx
        .load(
            &n("sales_invoice"),
            doc.id(),
            spot_storage_contract::RowLock::None,
        )
        .await
        .unwrap()
        .unwrap();
    let loaded = from_storage(&meta, stored).unwrap();
    assert_eq!(loaded.state(), Some(DocState::Cancelled));
    assert!(loaded.changed_fields().is_empty());
}

#[tokio::test]
async fn a_stored_state_that_is_not_a_state_is_an_internal_error() {
    let (backend, catalog) = backend().await;
    let meta = meta();
    let customer = RecordId::new_v7();
    insert_customer(&backend, &catalog, customer).await;
    let mut doc = valid_invoice();
    doc.set(n("customer"), customer);
    let mut data = to_storage(&meta, &doc).unwrap();
    data.fields
        .insert(n("doc_state"), Value::Text("archived".into()));
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    tx.insert(&data).await.unwrap();
    let stored = tx
        .load(
            &n("sales_invoice"),
            doc.id(),
            spot_storage_contract::RowLock::None,
        )
        .await
        .unwrap()
        .unwrap();
    let e = from_storage(&meta, stored).unwrap_err();
    assert_eq!(e.code, "document.invalid_state_value");
    assert_eq!(e.class, spot_app_api::ErrorClass::Internal);
}

#[test]
fn an_unknown_doctype_is_an_error_both_ways() {
    let meta = meta();
    let doc = Document::new(n("nope"), RecordId::new_v7());
    assert_eq!(
        to_storage(&meta, &doc).unwrap_err().code,
        "meta.unknown_doctype"
    );
    let stored = spot_storage_contract::Document {
        data: spot_storage_contract::DocumentData {
            entity: n("nope"),
            id: RecordId::new_v7(),
            fields: Default::default(),
            children: Default::default(),
        },
        version: Version::FIRST,
    };
    assert_eq!(
        from_storage(&meta, stored).unwrap_err().code,
        "meta.unknown_doctype"
    );
}
