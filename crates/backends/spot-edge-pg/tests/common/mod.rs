//! Shared setup for the edge-store integration tests: a scratch database, the tenant metadata
//! and its catalog, the edge schema, and document builders.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_edge_sync::{Edge, EdgeSchema};
use spot_graph_model::EdgeTypeId;
use spot_meta::{Customisations, Definition, FieldSpec, FieldType, Registry};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, DocumentStore, Fields, Isolation, Name, NodeRef, RecordId,
    RowLock, StorageBackend, Transaction, TxOptions, Value,
};

/// A scratch database, a backend on it, and the tenant metadata and catalog.
pub struct Env {
    /// The scratch database; dropped with the test.
    pub db: TestDb,
    /// The storage backend on it.
    pub backend: PgBackend,
    /// The catalog compiled from the metadata.
    pub catalog: Arc<Catalog>,
    /// The effective metadata.
    pub meta: spot_meta::EffectiveMeta,
}

impl Env {
    /// The edge schema derived from the metadata.
    #[must_use]
    pub fn schema(&self) -> EdgeSchema {
        EdgeSchema::derive(&self.meta, &[])
    }

    /// Creates this crate's tables in the scratch database.
    pub async fn prepare_edges(&self) {
        let mut client = self.db.raw_client().await;
        spot_edge_pg::schema::prepare(&mut client).await.unwrap();
    }

    /// Opens a read-write transaction on the catalog.
    pub async fn begin(&self, label: &str) -> PgTransaction {
        let options = TxOptions::new(n(label), Isolation::ReadCommitted);
        self.backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .unwrap()
    }

    /// Counts the rows of one table through a raw client.
    pub async fn count(&self, table: &str) -> i64 {
        let client = self.db.raw_client().await;
        client
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await
            .unwrap()
            .get(0)
    }
}

/// A scratch database with the tenant catalog and the edge tables prepared, or `None` without a
/// server (`SKIPPED:` is printed, exactly as the other `*-pg` crates do).
pub async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let config = db.config();
    let backend = PgBackend::connect(config).await.expect("connect");
    let meta = metadata();
    let catalog = Arc::new(
        meta.catalog(Collation::new("en").unwrap())
            .expect("catalog compiles"),
    );
    backend.prepare(&catalog).await.expect("prepare documents");
    let env = Env {
        db,
        backend,
        catalog,
        meta,
    };
    env.prepare_edges().await;
    Some(env)
}

/// A storage name.
pub fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

/// The tenant metadata: a `party` and an `invoice` linking to it.
pub fn metadata() -> spot_meta::EffectiveMeta {
    let party = Definition::new("party");
    let mut invoice = Definition::new("invoice");
    let mut customer = FieldSpec::new("party", FieldType::Link);
    customer.options = Some("party".into());
    invoice.fields = vec![customer, FieldSpec::new("number", FieldType::Text)];
    Registry::build(vec![party, invoice], &Customisations::new()).expect("metadata compiles")
}

/// A document of `entity` with every declared field null.
pub fn blank(env: &Env, entity: &str) -> DocumentData {
    let def = env.catalog.entity(&n(entity)).unwrap();
    let mut fields = Fields::new();
    for field in &def.fields {
        fields.insert(field.name.clone(), Value::Null);
    }
    DocumentData {
        entity: n(entity),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    }
}

/// A party.
pub fn party(env: &Env) -> DocumentData {
    blank(env, "party")
}

/// An invoice linking to `party`.
pub fn invoice(env: &Env, party_id: RecordId) -> DocumentData {
    let mut doc = blank(env, "invoice");
    doc.fields.insert(n("party"), Value::Id(party_id));
    doc.fields.insert(n("number"), Value::Text("INV-1".into()));
    doc
}

/// The one edge an invoice linking to a party implies.
#[must_use]
pub fn invoice_party_edge(invoice_id: RecordId, party_id: RecordId) -> Edge {
    Edge::new(
        EdgeTypeId::of("invoice.party"),
        NodeRef {
            entity: n("invoice"),
            id: invoice_id,
        },
        NodeRef {
            entity: n("party"),
            id: party_id,
        },
    )
}

/// Inserts `docs` in one committed transaction.
pub async fn seed(env: &Env, docs: &[&DocumentData]) {
    let mut tx = env.begin("edge_seed").await;
    for doc in docs {
        tx.insert(doc).await.unwrap();
    }
    tx.commit().await.unwrap();
}

/// Loads a document, or `None`.
pub async fn load(env: &Env, entity: &str, id: RecordId) -> Option<DocumentData> {
    let mut tx = env.begin("edge_load").await;
    let doc = tx.load(&n(entity), id, RowLock::None).await.unwrap();
    tx.rollback().await.unwrap();
    doc.map(|d| d.data)
}
