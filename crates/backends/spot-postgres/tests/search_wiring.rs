//! The search maintenance hook as `spot-postgres` calls it (WP-STO-13.T3, ADR-087 section 7).
//!
//! These tests use a recording fake, so they need no search projection and no extension: they
//! prove **when** the backend calls the hook (once per write, in order, with the right entity and
//! id, on the write's own transaction, never for an entity without a `search` declaration), that
//! a failing hook fails the write and leaves nothing behind, and that without a hook search is
//! refused exactly as before. The projection itself, end to end through `PgTransaction`, is
//! tested in `spot-pg-search/tests/search_wiring.rs`.
//!
//! Skipped without `SPOT_TEST_PG_URL`, required under `SPOT_REQUIRE_PG=1`, as the neighbours are.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use std::sync::{Arc, Mutex};

use documents_support::*;
use spot_postgres::{HookFuture, PgTransaction, SearchMaintenance};
use spot_storage_contract::{
    Catalog, DocumentStore, EntityDef, ErrorKind, Expr, FieldRef, Isolation, Query, QueryExecutor,
    QueryPath, ReadSource, RecordId, SearchDef, SearchField, SearchMode, SearchTerm, SearchWeight,
    Selection, StorageBackend, StorageError, Transaction, TxOptions, Version,
};

/// What the fake was asked to do, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Prepare(String),
    Write(String, RecordId),
    Delete(String, RecordId),
}

/// Records every call; fails `after_write` with `fail_with` when set.
#[derive(Default)]
struct Recording {
    calls: Mutex<Vec<Call>>,
    fail_with: Option<ErrorKind>,
}

impl Recording {
    fn failing(kind: ErrorKind) -> Self {
        Self {
            calls: Mutex::default(),
            fail_with: Some(kind),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl SearchMaintenance for Recording {
    fn serves_search(&self) -> bool {
        true
    }

    fn prepare_entity<'a>(
        &'a self,
        _tx: &'a mut PgTransaction,
        _catalog: &'a Catalog,
        entity: &'a EntityDef,
    ) -> HookFuture<'a, ()> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Prepare(entity.name.to_string()));
            Ok(())
        })
    }

    fn after_write<'a>(
        &'a self,
        _tx: &'a mut PgTransaction,
        entity: &'a EntityDef,
        id: RecordId,
    ) -> HookFuture<'a, ()> {
        Box::pin(async move {
            if let Some(kind) = self.fail_with {
                return Err(StorageError::new(kind, "the fake hook was told to fail"));
            }
            self.calls
                .lock()
                .unwrap()
                .push(Call::Write(entity.name.to_string(), id));
            Ok(())
        })
    }

    fn after_delete<'a>(
        &'a self,
        _tx: &'a mut PgTransaction,
        entity: &'a EntityDef,
        id: RecordId,
    ) -> HookFuture<'a, ()> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Delete(entity.name.to_string(), id));
            Ok(())
        })
    }
}

/// The contract catalog with a `search` declaration on `party.name`; `invoice` stays plain.
fn searchable_catalog() -> Arc<Catalog> {
    let base = catalog();
    let entities: Vec<EntityDef> = base
        .entities()
        .cloned()
        .map(|mut entity| {
            if entity.name == n("party") {
                entity.search = Some(SearchDef {
                    language: None,
                    fields: vec![SearchField {
                        name: n("name"),
                        weight: SearchWeight::A,
                    }],
                    facets: vec![],
                    typo_tolerance: false,
                });
            }
            entity
        })
        .collect();
    Arc::new(Catalog::new(base.version, base.collation.clone(), entities).unwrap())
}

async fn begin_on(backend: &spot_postgres::PgBackend, cat: &Arc<Catalog>) -> PgTransaction {
    let options = TxOptions::new(n("search_wiring"), Isolation::ReadCommitted);
    backend.begin(cat.clone(), options).await.unwrap()
}

fn search_query(text: &str) -> Query {
    let mut query = Query::from(&n("party"), &n("p"), QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::Field(FieldRef {
            alias: n("p"),
            field: n("id"),
        }),
        alias: n("id"),
    }];
    query.filter = Some(Expr::Search(SearchTerm {
        mode: SearchMode::Web,
        text: text.to_owned(),
        fields: vec![n("name")],
    }));
    query
}

async fn count(env: &Env, table: &str) -> i64 {
    let client = env.db.raw_client().await;
    client
        .query_one(&format!("SELECT count(*) FROM \"{table}\""), &[])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn prepare_asks_the_hook_for_each_searchable_entity_and_only_those() {
    let Some(env) = common::env().await else {
        return;
    };
    let hook = Arc::new(Recording::default());
    let backend = env.backend.clone().with_search(hook.clone());
    backend.prepare(&searchable_catalog()).await.unwrap();
    assert_eq!(hook.calls(), vec![Call::Prepare("party".to_owned())]);
    assert!(backend.capabilities().search);
    // A hook-less clone of the same pool still says no.
    assert!(!env.backend.capabilities().search);
    // Preparing again asks again (the hook decides what is already there), and a catalog with
    // no search declaration never reaches the hook.
    hook.take();
    backend.prepare(&catalog()).await.unwrap();
    assert!(hook.calls().is_empty());
}

#[tokio::test]
async fn a_write_calls_the_hook_once_in_order_and_a_plain_entity_costs_nothing() {
    let Some(env) = common::env().await else {
        return;
    };
    let hook = Arc::new(Recording::default());
    let backend = env.backend.clone().with_search(hook.clone());
    let cat = searchable_catalog();
    backend.prepare(&cat).await.unwrap();
    hook.take();

    let first = party("Lighthouse", 0);
    let second = party("Orchard", 0);
    let mut tx = begin_on(&backend, &cat).await;
    let v1 = tx.insert(&first).await.unwrap();
    tx.insert(&second).await.unwrap();
    // The invoice entity has no search declaration: no hook call.
    let inv = invoice("INV-1", first.id, vec![line("a", 1_000, 100)]);
    tx.insert(&inv).await.unwrap();
    let mut changed = first.clone();
    changed.fields.insert(n("name"), "Lighthouse keeper".into());
    tx.update(&changed, v1).await.unwrap();
    tx.delete(&n("party"), second.id, Version::FIRST)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        hook.calls(),
        vec![
            Call::Write("party".to_owned(), first.id),
            Call::Write("party".to_owned(), second.id),
            Call::Write("party".to_owned(), first.id),
            Call::Delete("party".to_owned(), second.id),
        ]
    );
}

#[tokio::test]
async fn a_failed_row_write_never_reaches_the_hook() {
    let Some(env) = common::env().await else {
        return;
    };
    let hook = Arc::new(Recording::default());
    let backend = env.backend.clone().with_search(hook.clone());
    let cat = searchable_catalog();
    backend.prepare(&cat).await.unwrap();
    hook.take();

    let doc = party("Orchard", 0);
    let mut tx = begin_on(&backend, &cat).await;
    tx.insert(&doc).await.unwrap();
    // The same id again: the row write fails, so the hook must not be told about a second write.
    let again = tx.insert(&doc).await.unwrap_err();
    assert_eq!(again.kind, ErrorKind::AlreadyExists, "{again}");
    tx.rollback().await.unwrap();
    assert_eq!(hook.calls(), vec![Call::Write("party".to_owned(), doc.id)]);
    assert_eq!(count(&env, "party").await, 0, "dropped, so rolled back");
}

#[tokio::test]
async fn a_failing_hook_fails_the_write_and_a_rollback_leaves_no_document() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = searchable_catalog();
    env.backend
        .clone()
        .with_search(Arc::new(Recording::default()))
        .prepare(&cat)
        .await
        .unwrap();
    let backend = env
        .backend
        .clone()
        .with_search(Arc::new(Recording::failing(ErrorKind::Internal)));

    let doc = party("Lighthouse", 0);
    let mut tx = begin_on(&backend, &cat).await;
    let error = tx.insert(&doc).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Internal, "{error}");
    // Poisoned: nothing more is sent, and the error says why.
    let next = tx.insert(&party("Other", 0)).await.unwrap_err();
    assert_eq!(next.kind, ErrorKind::Internal, "{next}");
    tx.rollback().await.unwrap();
    assert_eq!(count(&env, "party").await, 0);
}

#[tokio::test]
async fn a_recoverable_hook_failure_is_undone_by_the_savepoint_it_ran_in() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = searchable_catalog();
    env.backend
        .clone()
        .with_search(Arc::new(Recording::default()))
        .prepare(&cat)
        .await
        .unwrap();
    let backend = env
        .backend
        .clone()
        .with_search(Arc::new(Recording::failing(ErrorKind::InvalidValue)));

    let lost = party("Lost", 0);
    let mut tx = begin_on(&backend, &cat).await;
    let savepoint = tx.savepoint().await.unwrap();
    let error = tx.insert(&lost).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidValue, "{error}");
    // The write is refused until the savepoint is rolled back, and rolling back undoes the row
    // write that preceded the hook's failure.
    assert!(tx.insert(&party("Early", 0)).await.is_err());
    tx.rollback_to(savepoint).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count(&env, "party").await, 0, "the half-done write is gone");
}

#[tokio::test]
async fn without_a_hook_search_is_unsupported_and_prepare_refuses_the_declaration() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = searchable_catalog();
    assert!(!env.backend.capabilities().search);

    let error = env.backend.prepare(&cat).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error}");
    assert!(
        error
            .explanation
            .contains("no search maintenance is installed"),
        "{error}"
    );
    assert!(!error.explanation.contains("arrives with"), "{error}");
    assert_eq!(
        count_tables(&env).await,
        0,
        "a refused prepare changes nothing"
    );

    // The same refusal on the read paths, once the tables exist (prepared through a hook).
    env.backend
        .clone()
        .with_search(Arc::new(Recording::default()))
        .prepare(&cat)
        .await
        .unwrap();
    let query = search_query("lighthouse");
    let mut tx = begin_on(&env.backend, &cat).await;
    let error = tx.query(&query).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error}");
    tx.rollback().await.unwrap();
    let error = env
        .backend
        .read(cat.clone(), ReadSource::Primary, &query)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error}");
}

async fn count_tables(env: &Env) -> i64 {
    let client = env.db.raw_client().await;
    client
        .query_one(
            "SELECT count(*) FROM pg_tables WHERE tablename = 'party'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}
