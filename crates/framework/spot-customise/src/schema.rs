//! The schema step of the pipeline: plan a catalogue, refuse the destructive, apply the rest.
//!
//! [`SchemaBackend`] is the one seam to the database. [`StorageSchema`] drives any storage
//! backend through its additive `prepare` (the fake in tests, any backend that supports
//! `prepare`); the optional `postgres` feature adds [`PgSchema`](crate::pg::PgSchema), which plans and
//! applies with the online migration engine. Both classify steps the same way: **additive**,
//! **needs backfill** (a required field on a table with rows) or **refused** (destructive or
//! lossy). A plan with any refused step is never applied.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_storage_contract::{
    AggFunc, Catalog, DocumentData, DocumentStore, EntityDef, Expr, FieldDef, Isolation, Name,
    Query, QueryExecutor, QueryPath, ReadSource, RecordId, RowLock, Selection, StorageBackend,
    Transaction, TxOptions, Value, Wait,
};

/// The future a [`SchemaBackend`] call returns. The error is a plain sentence: it names the
/// problem and never carries SQL.
pub type SchemaFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// How risky a planned step is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepClass {
    /// Safe and online; no data at risk.
    Additive,
    /// Part of filling a new required column on a table that holds rows.
    NeedsBackfill,
    /// Validates every existing row while it blocks writes (an exclusion constraint added to a
    /// table that holds rows). It is never treated as plain additive.
    NeedsValidation,
}

/// One planned step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanStep {
    /// A stable identity, for example `add_field:invoice.custom_note`.
    pub key: String,
    /// The class.
    pub class: StepClass,
    /// What it does, in one sentence.
    pub sentence: String,
}

/// One destructive or lossy change the plan refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusedStep {
    /// The entity concerned.
    pub entity: String,
    /// The field concerned, when the change is about one.
    pub field: Option<String>,
    /// The named reason, one sentence.
    pub reason: String,
}

impl fmt::Display for RefusedStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.field {
            Some(field) => write!(f, "`{}.{field}`: {}", self.entity, self.reason),
            None => write!(f, "`{}`: {}", self.entity, self.reason),
        }
    }
}

/// The steps that take the database from where it is to a catalogue, and the changes refused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchemaPlan {
    /// The ordered steps.
    pub steps: Vec<PlanStep>,
    /// The refused changes. Non-empty means the plan is never applied.
    pub refused: Vec<RefusedStep>,
}

impl SchemaPlan {
    /// Whether there is nothing to do and nothing refused.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.refused.is_empty()
    }
}

/// A value to write into the rows a new custom field found already there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fill {
    /// The DocType (its table).
    pub doctype: String,
    /// The new field (its column).
    pub field: String,
    /// The field's default.
    pub value: Value,
}

/// The database side of the pipeline.
pub trait SchemaBackend: Send + Sync {
    /// Plans the change to `wanted` without side effects.
    fn plan<'a>(&'a self, wanted: &'a Catalog) -> SchemaFuture<'a, SchemaPlan>;

    /// Applies `wanted` additively and idempotently: a rerun after a crash does only what is
    /// left. Takes the backend's own locks per step; the caller holds none.
    fn apply<'a>(&'a self, wanted: &'a Catalog) -> SchemaFuture<'a, ()>;

    /// Whether the DocType's table holds any rows (the `NeedsBackfill` decision).
    fn has_rows<'a>(&'a self, doctype: &'a str) -> SchemaFuture<'a, bool>;

    /// Writes each fill's value into the rows where the column is still null, in bounded
    /// batches, and returns how many rows it filled. Idempotent: a rerun fills only what is
    /// left, so recovery may call it again.
    fn backfill<'a>(&'a self, wanted: &'a Catalog, fills: &'a [Fill]) -> SchemaFuture<'a, u64>;
}

/// Rows a backfill batch updates in one transaction.
const BACKFILL_BATCH: u32 = 1_000;

/// Fills `fill.field` of `fill.doctype` with `fill.value` where it is null, through the storage
/// contract's document operations (so it works on every backend), `BACKFILL_BATCH` rows per
/// transaction with the rows locked for the update. Returns the number of rows filled.
///
/// # Errors
///
/// A sentence describing the storage failure; rows filled so far stay filled (a rerun resumes).
pub async fn backfill_documents<B: StorageBackend>(
    backend: &B,
    catalog: Arc<Catalog>,
    fill: &Fill,
) -> Result<u64, String> {
    let storage = |e: spot_storage_contract::StorageError| e.to_string();
    let entity = Name::new(&fill.doctype).map_err(storage)?;
    let field = Name::new(&fill.field).map_err(storage)?;
    let alias = Name::new("t").map_err(storage)?;
    let id_column = Name::new("id").map_err(storage)?;
    let mut filled = 0u64;
    loop {
        let options = TxOptions::new(
            Name::new("customise_backfill").map_err(storage)?,
            Isolation::ReadCommitted,
        );
        let mut tx = backend
            .begin(Arc::clone(&catalog), options)
            .await
            .map_err(storage)?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        query.select.push(Selection {
            expr: Expr::field(&alias, &id_column),
            alias: id_column.clone(),
        });
        query.filter = Some(Expr::IsNull(Box::new(Expr::field(&alias, &field))));
        query.limit = Some(BACKFILL_BATCH);
        query.lock = RowLock::Update(Wait::Block);
        let rows = tx.query(&query).await.map_err(storage)?;
        let ids: Vec<RecordId> = rows
            .rows
            .iter()
            .filter_map(|row| match row.first() {
                Some(Value::Id(id)) => Some(*id),
                _ => None,
            })
            .collect();
        if ids.is_empty() {
            tx.rollback().await.map_err(storage)?;
            return Ok(filled);
        }
        for id in &ids {
            let Some(document) = tx
                .load(&entity, *id, RowLock::None)
                .await
                .map_err(storage)?
            else {
                continue;
            };
            let mut data: DocumentData = document.data;
            data.fields.insert(field.clone(), fill.value.clone());
            tx.update(&data, document.version).await.map_err(storage)?;
        }
        tx.commit().await.map_err(storage)?;
        filled += u64::try_from(ids.len()).unwrap_or(u64::MAX);
    }
}

/// Diffs two catalogues the way an additive backend sees them.
///
/// New entities and fields are additive steps. A removed entity or field, a changed type, or a
/// required field that was nullable is refused with a named reason. Fields are compared by
/// name; `old` is what the database holds, `new` what the metadata now wants.
#[must_use]
pub fn diff_catalogs(old: &Catalog, new: &Catalog) -> SchemaPlan {
    let mut plan = SchemaPlan::default();
    let wanted: BTreeMap<&Name, &EntityDef> = new.entities().map(|e| (&e.name, e)).collect();
    let held: BTreeMap<&Name, &EntityDef> = old.entities().map(|e| (&e.name, e)).collect();

    for (name, entity) in &wanted {
        match held.get(name) {
            None => plan.steps.push(PlanStep {
                key: format!("create_entity:{name}"),
                class: StepClass::Additive,
                sentence: format!(
                    "Create the table for `{name}` with {} field(s).",
                    entity.fields.len()
                ),
            }),
            Some(existing) => diff_fields(existing, entity, &mut plan),
        }
    }
    for name in held.keys() {
        if !wanted.contains_key(name) {
            plan.refused.push(RefusedStep {
                entity: name.to_string(),
                field: None,
                reason: "dropping a DocType's table is destructive".to_owned(),
            });
        }
    }
    if !plan.steps.is_empty() {
        plan.steps.push(PlanStep {
            key: "finalise".to_owned(),
            class: StepClass::Additive,
            sentence: "Record the new catalogue version.".to_owned(),
        });
    }
    plan
}

fn diff_fields(existing: &EntityDef, entity: &EntityDef, plan: &mut SchemaPlan) {
    let held: BTreeMap<&Name, &FieldDef> = existing.fields.iter().map(|f| (&f.name, f)).collect();
    let wanted: BTreeMap<&Name, &FieldDef> = entity.fields.iter().map(|f| (&f.name, f)).collect();
    for field in &entity.fields {
        match held.get(&field.name) {
            None => plan.steps.push(PlanStep {
                key: format!("add_field:{}.{}", entity.name, field.name),
                class: if field.nullable {
                    StepClass::Additive
                } else {
                    StepClass::NeedsBackfill
                },
                sentence: format!("Add the field `{}` to `{}`.", field.name, entity.name),
            }),
            Some(old) if old.ty != field.ty => plan.refused.push(RefusedStep {
                entity: entity.name.to_string(),
                field: Some(field.name.to_string()),
                reason: "changing a field's type is lossy".to_owned(),
            }),
            Some(old) if old.nullable && !field.nullable => plan.refused.push(RefusedStep {
                entity: entity.name.to_string(),
                field: Some(field.name.to_string()),
                reason: "tightening a field to not-null is refused without a backfill path"
                    .to_owned(),
            }),
            Some(_) => {}
        }
    }
    for name in held.keys() {
        if !wanted.contains_key(name) {
            plan.refused.push(RefusedStep {
                entity: entity.name.to_string(),
                field: Some(name.to_string()),
                reason: "dropping a field removes data".to_owned(),
            });
        }
    }
}

/// A [`SchemaBackend`] over any storage backend's additive `prepare`.
///
/// It remembers the last catalogue it applied (the database's shape) and plans by diffing the
/// wanted catalogue against it. This is the fake in the tests; against PostgreSQL use
/// [`PgSchema`](crate::pg::PgSchema) (feature `postgres`).
pub struct StorageSchema<B: StorageBackend> {
    backend: Arc<B>,
    applied: Mutex<Arc<Catalog>>,
}

impl<B: StorageBackend> StorageSchema<B> {
    /// A schema over `backend`, whose tables already match `current`.
    #[must_use]
    pub fn new(backend: Arc<B>, current: Catalog) -> Self {
        Self {
            backend,
            applied: Mutex::new(Arc::new(current)),
        }
    }

    /// The catalogue the database currently matches.
    #[must_use]
    pub fn applied(&self) -> Arc<Catalog> {
        Arc::clone(&self.lock())
    }

    fn lock(&self) -> MutexGuard<'_, Arc<Catalog>> {
        self.applied
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<B: StorageBackend> SchemaBackend for StorageSchema<B> {
    fn plan<'a>(&'a self, wanted: &'a Catalog) -> SchemaFuture<'a, SchemaPlan> {
        let plan = diff_catalogs(&self.applied(), wanted);
        Box::pin(async move { Ok(plan) })
    }

    fn apply<'a>(&'a self, wanted: &'a Catalog) -> SchemaFuture<'a, ()> {
        Box::pin(async move {
            let plan = diff_catalogs(&self.applied(), wanted);
            if !plan.refused.is_empty() {
                return Err("the plan holds a refused step; nothing was applied".to_owned());
            }
            self.backend
                .prepare(wanted)
                .await
                .map_err(|e| e.to_string())?;
            *self.lock() = Arc::new(wanted.clone());
            Ok(())
        })
    }

    fn has_rows<'a>(&'a self, doctype: &'a str) -> SchemaFuture<'a, bool> {
        Box::pin(async move {
            let catalog = self.applied();
            let entity = Name::new(doctype).map_err(|e| e.to_string())?;
            if catalog.entity(&entity).is_err() {
                return Ok(false);
            }
            let alias = Name::new("t").map_err(|e| e.to_string())?;
            let count = Name::new("n").map_err(|e| e.to_string())?;
            let mut query = Query::from(&entity, &alias, QueryPath::Report);
            query.select.push(Selection {
                expr: Expr::Agg {
                    func: AggFunc::Count,
                    arg: None,
                    distinct: false,
                },
                alias: count,
            });
            let rows = self
                .backend
                .read(catalog, ReadSource::Primary, &query)
                .await
                .map_err(|e| e.to_string())?;
            Ok(rows
                .rows
                .first()
                .and_then(|row| row.first())
                .is_some_and(|value| matches!(value, Value::Int(n) if *n > 0)))
        })
    }

    fn backfill<'a>(&'a self, wanted: &'a Catalog, fills: &'a [Fill]) -> SchemaFuture<'a, u64> {
        Box::pin(self.fill_all(wanted, fills))
    }
}

impl<B: StorageBackend> StorageSchema<B> {
    async fn fill_all(&self, wanted: &Catalog, fills: &[Fill]) -> Result<u64, String> {
        let catalog = Arc::new(wanted.clone());
        let mut total = 0;
        for fill in fills {
            total += backfill_documents(&*self.backend, Arc::clone(&catalog), fill).await?;
        }
        Ok(total)
    }
}

impl<B: StorageBackend> fmt::Debug for StorageSchema<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageSchema").finish_non_exhaustive()
    }
}
