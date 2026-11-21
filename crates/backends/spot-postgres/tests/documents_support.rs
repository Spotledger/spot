//! Builders and helpers shared by the schema and document tests: the contract suite's catalog
//! (parties, invoices, invoice lines) and documents over it.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use spot_postgres::PgTransaction;
use spot_storage_contract::contract_tests;
use spot_storage_contract::{
    Catalog, ChildRow, DocumentData, DocumentStore, ErrorKind, Fields, Isolation, Name, RecordId,
    RowLock, StorageBackend, StorageError, StorageResult, Transaction, TxOptions, Value, Version,
};

#[path = "common/mod.rs"]
pub mod common;

pub use common::Env;

pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

pub fn catalog() -> Arc<Catalog> {
    Arc::new(contract_tests::catalog().unwrap())
}

/// A scratch database with the contract catalog prepared, or `None` without a server.
pub async fn prepared() -> Option<(Env, Arc<Catalog>)> {
    let env = common::env().await?;
    let cat = catalog();
    env.backend.prepare(&cat).await.unwrap();
    Some((env, cat))
}

pub async fn begin(env: &Env, cat: &Arc<Catalog>) -> PgTransaction {
    begin_with(env, cat, |_| {}).await
}

pub async fn begin_with(
    env: &Env,
    cat: &Arc<Catalog>,
    tweak: impl FnOnce(&mut TxOptions),
) -> PgTransaction {
    let mut options = TxOptions::new(n("documents_test"), Isolation::ReadCommitted);
    tweak(&mut options);
    env.backend.begin(cat.clone(), options).await.unwrap()
}

/// Commits `docs` in one transaction.
pub async fn seed(env: &Env, cat: &Arc<Catalog>, docs: &[&DocumentData]) {
    let mut tx = begin(env, cat).await;
    for doc in docs {
        tx.insert(doc).await.unwrap();
    }
    tx.commit().await.unwrap();
}

/// Loads a document in its own transaction.
pub async fn load(
    env: &Env,
    cat: &Arc<Catalog>,
    entity: &str,
    id: RecordId,
) -> Option<(DocumentData, Version)> {
    let mut tx = begin(env, cat).await;
    let doc = tx.load(&n(entity), id, RowLock::None).await.unwrap();
    tx.rollback().await.unwrap();
    doc.map(|d| (d.data, d.version))
}

pub fn money(cents: i64) -> Value {
    Value::Decimal(Decimal::new(cents, 2))
}

/// A party with every optional field null.
pub fn party(name: &str, limit_cents: i64) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("name"), name.into());
    fields.insert(n("credit_limit"), money(limit_cents));
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

pub fn line(item: &str, qty_milli: i64, cents: i64) -> ChildRow {
    let mut fields = Fields::new();
    fields.insert(n("item"), item.into());
    fields.insert(n("qty"), Value::Decimal(Decimal::new(qty_milli, 3)));
    fields.insert(n("amount"), money(cents));
    ChildRow {
        id: RecordId::new_v7(),
        fields,
    }
}

pub fn invoice(number: &str, party: RecordId, lines: Vec<ChildRow>) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("number"), number.into());
    fields.insert(n("party"), Value::Id(party));
    fields.insert(n("total"), money(0));
    fields.insert(n("ref_entity"), Value::Null);
    fields.insert(n("ref_id"), Value::Null);
    let mut children = BTreeMap::new();
    children.insert(n("lines"), lines);
    DocumentData {
        entity: n("invoice"),
        id: RecordId::new_v7(),
        fields,
        children,
    }
}

pub fn lines_of(doc: &DocumentData) -> &Vec<ChildRow> {
    &doc.children[&n("lines")]
}

pub fn item_of(row: &ChildRow) -> String {
    match &row.fields[&n("item")] {
        Value::Text(s) => s.clone(),
        other => panic!("item is {other:?}"),
    }
}

pub fn expect_kind<T: std::fmt::Debug>(r: StorageResult<T>, kind: ErrorKind) -> StorageError {
    match r {
        Err(e) if e.kind == kind => e,
        other => panic!("expected {}, got {other:?}", kind.code()),
    }
}
