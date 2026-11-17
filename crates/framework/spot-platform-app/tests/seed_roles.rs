//! `seed_system_rows`: the migration-time seed writes only the missing system roles: no user, no
//! assignment, no credential, no overwrite of an edited row, and nothing the second time.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{collation, meta};
use spot_platform_app::bootstrap::{SeedParams, seed, seed_system_rows};
use spot_platform_app::{doctype, role};
use spot_storage_contract::{
    Catalog, DocumentStore, Expr, FieldRef, Isolation, Name, Query, QueryExecutor, QueryPath,
    RecordId, RowLock, Selection, StorageBackend, Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn options() -> TxOptions {
    TxOptions::new(n("seed_test"), Isolation::ReadCommitted)
}

async fn installed() -> (FakeBackend, Arc<Catalog>) {
    let catalog = Arc::new(meta().catalog(collation()).unwrap());
    let backend = FakeBackend::new();
    backend.prepare(&catalog).await.unwrap();
    (backend, catalog)
}

/// The values of one column of every stored document of `entity`.
async fn column(tx: &mut impl QueryExecutor, entity: &str, field: &str) -> Vec<Value> {
    let alias = n("t");
    let mut q = Query::from(&n(entity), &alias, QueryPath::Report);
    q.select = vec![Selection {
        expr: Expr::Field(FieldRef {
            alias,
            field: n(field),
        }),
        alias: n("v"),
    }];
    let rows = tx.query(&q).await.unwrap_or_else(|e| panic!("{e}"));
    rows.rows.into_iter().flatten().collect()
}

async fn count(backend: &FakeBackend, catalog: &Arc<Catalog>, entity: &str) -> usize {
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let len = column(&mut tx, entity, "id").await.len();
    tx.rollback().await.unwrap();
    len
}

/// The id of the seeded role called `name`.
async fn role_id(backend: &FakeBackend, catalog: &Arc<Catalog>, name: &str) -> RecordId {
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let alias = n("t");
    let mut q = Query::from(&n(doctype::ROLE), &alias, QueryPath::Report);
    q.select = vec![
        Selection {
            expr: Expr::Field(FieldRef {
                alias: alias.clone(),
                field: n("id"),
            }),
            alias: n("i"),
        },
        Selection {
            expr: Expr::Field(FieldRef {
                alias,
                field: n("role_name"),
            }),
            alias: n("v"),
        },
    ];
    let rows = tx.query(&q).await.unwrap();
    tx.rollback().await.unwrap();
    for row in rows.rows {
        if let [Value::Id(id), Value::Text(found)] = row.as_slice()
            && found == name
        {
            return *id;
        }
    }
    panic!("no role {name}");
}

#[tokio::test]
async fn on_an_empty_store_it_writes_the_roles_and_no_user_and_no_assignment() {
    let (backend, catalog) = installed().await;
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let outcome = seed_system_rows(&mut tx, &SeedParams::default())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(outcome.written(), 5);
    assert!(
        outcome
            .roles_added
            .contains(&role::WEBHOOK_MANAGER.to_owned())
    );
    assert_eq!(count(&backend, &catalog, doctype::ROLE).await, 5);
    assert_eq!(
        count(&backend, &catalog, doctype::USER).await,
        0,
        "no identity is created"
    );
    assert_eq!(count(&backend, &catalog, doctype::ROLE_ASSIGNMENT).await, 0);
}

#[tokio::test]
async fn a_site_with_the_old_roles_gains_exactly_the_missing_one_and_edits_survive() {
    let (backend, catalog) = installed().await;
    let params = SeedParams::default();
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    seed(&mut tx, &params).await.unwrap();
    tx.commit().await.unwrap();
    // Simulate a site older than the Webhook Manager role.
    let webhook = role_id(&backend, &catalog, role::WEBHOOK_MANAGER).await;
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let doomed = tx
        .load(&n(doctype::ROLE), webhook, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    tx.delete(&n(doctype::ROLE), webhook, doomed.version)
        .await
        .unwrap();
    // An edited description on another seeded role.
    let manager = role_id(&backend, &catalog, role::USER_MANAGER).await;
    let mut edited = tx
        .load(&n(doctype::ROLE), manager, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    edited
        .data
        .fields
        .insert(n("description"), Value::from("edited by the operator"));
    tx.update(&edited.data, edited.version).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let outcome = seed_system_rows(&mut tx, &params).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(outcome.roles_added, [role::WEBHOOK_MANAGER]);
    assert_eq!(
        role_id(&backend, &catalog, role::WEBHOOK_MANAGER).await,
        webhook,
        "fixed id"
    );
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let kept = tx
        .load(&n(doctype::ROLE), manager, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        kept.data.fields[&n("description")],
        Value::from("edited by the operator")
    );
}

#[tokio::test]
async fn a_second_run_writes_nothing_and_seed_after_it_writes_only_the_identity() {
    let (backend, catalog) = installed().await;
    let params = SeedParams::default();
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    assert_eq!(
        seed_system_rows(&mut tx, &params).await.unwrap().written(),
        5
    );
    tx.commit().await.unwrap();
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    assert_eq!(
        seed_system_rows(&mut tx, &params).await.unwrap().written(),
        0
    );
    // The full seed then writes the Administrator and its assignment only.
    let outcome = seed(&mut tx, &params).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(outcome.written, 2);
    assert_eq!(count(&backend, &catalog, doctype::ROLE).await, 5);
}
