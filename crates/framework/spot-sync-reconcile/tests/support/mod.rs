//! Test support: a `DigestSource` over the storage fake, and a small catalog to fill.
//!
//! The reference read in the crate (`MemorySource`) is already in memory. This module is the
//! second reference the brief asks for: a source that reads a node's **storage** — here the
//! in-memory backend of the storage contract, `spot-storage-fake` — as one consistent snapshot.
//! It is the shape a PostgreSQL-backed source takes: read the safe bookmark, then read the
//! scope's documents inside a `RepeatableRead` transaction so in-flight writes never leak in.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;

use spot_storage_contract::{
    Catalog, ChangeFeed, ChangeOp, ChildTableDef, Collation, DocumentData, DocumentStore,
    EntityDef, EntityKind, FeedPosition, FieldDef, FieldType, Isolation, Name, RecordId, RowLock,
    StorageBackend, Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;
use spot_sync_reconcile::{
    DigestPlan, DigestSource, GroupSpec, ReconcileError, ReconcileResult, RecordFacts, RecordKey,
    Scope, ScopeSnapshot,
};

/// A `DigestSource` that reads a `FakeBackend` through the storage contract.
pub struct FakeSource {
    /// The node's store.
    pub backend: FakeBackend,
    /// The catalog the store was prepared with.
    pub catalog: Arc<Catalog>,
    /// Which entities and fields the scope covers.
    pub plan: DigestPlan,
    /// The scope this node holds.
    pub scope: Scope,
}

impl FakeSource {
    /// A source over `backend` and `catalog` for `scope`, following `plan`.
    pub fn new(
        backend: FakeBackend,
        catalog: Arc<Catalog>,
        plan: DigestPlan,
        scope: Scope,
    ) -> Self {
        Self {
            backend,
            catalog,
            plan,
            scope,
        }
    }
}

impl DigestSource for FakeSource {
    async fn snapshot(&self, scope: &Scope) -> ReconcileResult<ScopeSnapshot> {
        if scope != &self.scope {
            return Err(ReconcileError::InvalidScope {
                reason: format!("the source holds {}", self.scope.label()),
            });
        }
        let (changes, bookmark) = read_all_changes(&self.backend).await?;
        let options = TxOptions::new(
            Name::new("sync_reconcile_read").unwrap(),
            Isolation::RepeatableRead,
        )
        .read_only();
        let mut tx = self
            .backend
            .begin(self.catalog.clone(), options)
            .await
            .map_err(source_failed)?;
        let mut records = Vec::new();
        for change in changes {
            if change.op == ChangeOp::Delete {
                continue;
            }
            if !self.plan.entities().contains(&change.document.entity) {
                continue;
            }
            let Some(document) = tx
                .load(&change.document.entity, change.document.id, RowLock::None)
                .await
                .map_err(source_failed)?
            else {
                continue;
            };
            records.push(facts_of(&self.plan, &document));
        }
        tx.commit().await.map_err(source_failed)?;
        Ok(ScopeSnapshot::new(scope.clone(), bookmark, records))
    }
}

fn source_failed(error: spot_storage_contract::StorageError) -> ReconcileError {
    ReconcileError::Source {
        message: error.to_string(),
    }
}

/// Every change the store has published, and the safe bookmark it reached.
async fn read_all_changes(
    backend: &FakeBackend,
) -> ReconcileResult<(Vec<spot_storage_contract::Change>, FeedPosition)> {
    const BATCH: u32 = 1_000;
    let mut after = FeedPosition::START;
    let mut all = Vec::new();
    loop {
        let batch = backend
            .read_changes(after, BATCH)
            .await
            .map_err(source_failed)?;
        let count = batch.len();
        if let Some(last) = batch.last() {
            after = last.position;
        }
        all.extend(batch);
        if count < BATCH as usize {
            break;
        }
    }
    Ok((all, after))
}

/// The facts one loaded document contributes to the plan's digest.
fn facts_of(plan: &DigestPlan, document: &spot_storage_contract::Document) -> RecordFacts {
    let entity = document.data.entity.clone();
    let name = match document.data.fields.get(&plan.name_field) {
        Some(Value::Text(text)) => text.clone(),
        _ => document.data.id.to_string(),
    };
    let mut facts = RecordFacts::of(RecordKey::new(entity.clone(), name, document.version));
    for (total_entity, field) in &plan.totals {
        if total_entity == &entity
            && let Some(Value::Int(value)) = document.data.fields.get(field)
        {
            facts.totals.push((field.clone(), i128::from(*value)));
        }
    }
    if let Some(gl) = &plan.gl
        && gl.entity == entity
        && let (Some(Value::Text(account)), Some(Value::Int(value))) = (
            document.data.fields.get(&gl.key_field),
            document.data.fields.get(&gl.value_field),
        )
    {
        facts.gl.push((account.clone(), i128::from(*value)));
    }
    if let Some(stock) = &plan.stock
        && stock.entity == entity
        && let (Some(Value::Text(item)), Some(Value::Int(value))) = (
            document.data.fields.get(&stock.key_field),
            document.data.fields.get(&stock.value_field),
        )
    {
        facts.stock.push((item.clone(), i128::from(*value)));
    }
    facts
}

/// A name, for building a catalog in a test.
pub fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

/// A text field.
pub fn text(field: &str) -> FieldDef {
    FieldDef {
        name: name(field),
        ty: FieldType::Text { max_chars: None },
        nullable: false,
    }
}

/// An integer field.
pub fn int(field: &str) -> FieldDef {
    FieldDef {
        name: name(field),
        ty: FieldType::Int,
        nullable: false,
    }
}

/// A document entity with the given fields.
pub fn entity(field_name: &str, fields: Vec<FieldDef>) -> EntityDef {
    EntityDef {
        name: name(field_name),
        kind: EntityKind::Document,
        fields,
        children: Vec::<ChildTableDef>::new(),
        indexes: Vec::new(),
        exclusions: vec![],
        search: None,
    }
}

/// The catalog every test in this module uses: items, invoices, GL entries and stock ledger.
pub fn catalog() -> Arc<Catalog> {
    Arc::new(
        Catalog::new(
            1,
            Collation::new("en").unwrap(),
            vec![
                entity("item", vec![text("name"), int("qty")]),
                entity("invoice", vec![text("name"), int("total")]),
                entity(
                    "gl_entry",
                    vec![text("name"), text("account"), int("amount")],
                ),
                entity("stock_ledger", vec![text("name"), text("item"), int("qty")]),
            ],
        )
        .unwrap(),
    )
}

/// The plan matching [`catalog`].
pub fn plan() -> DigestPlan {
    DigestPlan::new(name("name"))
        .counting(name("item"))
        .counting(name("invoice"))
        .totalling(name("invoice"), name("total"))
        .with_gl(GroupSpec::new(
            name("gl_entry"),
            name("account"),
            name("amount"),
        ))
        .with_stock(GroupSpec::new(
            name("stock_ledger"),
            name("item"),
            name("qty"),
        ))
}

/// A field map from `(field, value)` pairs.
pub fn fields(pairs: &[(&str, Value)]) -> std::collections::BTreeMap<Name, Value> {
    pairs
        .iter()
        .map(|(field, value)| (name(field), value.clone()))
        .collect()
}

/// Inserts one document into a store.
pub async fn insert(
    backend: &FakeBackend,
    catalog: &Arc<Catalog>,
    entity: &str,
    id: RecordId,
    values: std::collections::BTreeMap<Name, Value>,
) {
    let options = TxOptions::new(name("seed"), Isolation::ReadCommitted);
    let mut tx = backend.begin(catalog.clone(), options).await.unwrap();
    tx.insert(&DocumentData {
        entity: name(entity),
        id,
        fields: values,
        children: std::collections::BTreeMap::new(),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// A fresh, prepared store with the test catalog.
pub async fn store() -> (FakeBackend, Arc<Catalog>) {
    let backend = FakeBackend::new();
    let catalog = catalog();
    backend.prepare(&catalog).await.unwrap();
    (backend, catalog)
}

/// Runs a future to completion on a fresh current-thread runtime.
pub fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}
