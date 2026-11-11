//! `bootstrap::seed`: creates the Administrator and the system roles once, writes no secret, and
//! fails cleanly when the DocTypes are absent.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{collation, meta};
use spot_platform_app::bootstrap::{SeedError, SeedParams, seed};
use spot_platform_app::{definitions, doctype, role};
use spot_storage_contract::{
    Catalog, DocumentStore, Expr, FieldRef, Isolation, Name, Query, QueryExecutor, QueryPath,
    RowLock, Selection, StorageBackend, Transaction, TxOptions, Value,
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

#[tokio::test]
async fn seeding_twice_gives_one_administrator() {
    let (backend, catalog) = installed().await;
    let params = SeedParams::default();

    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let first = seed(&mut tx, &params).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(first.written, 6, "four roles, the user and the assignment");

    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let second = seed(&mut tx, &params).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(second.written, 0);
    assert_eq!(second.administrator, first.administrator);

    assert_eq!(count(&backend, &catalog, doctype::USER).await, 1);
    assert_eq!(count(&backend, &catalog, doctype::ROLE).await, 4);
    assert_eq!(count(&backend, &catalog, doctype::ROLE_ASSIGNMENT).await, 1);
}

#[tokio::test]
async fn the_administrator_has_no_credential_and_holds_the_tenant_administrator_role() {
    let (backend, catalog) = installed().await;
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let outcome = seed(&mut tx, &SeedParams::default()).await.unwrap();
    let admin = tx
        .load(&n(doctype::USER), outcome.administrator, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    // Secrets have no column of their own, so the stored document cannot hold one; the visible
    // state is an invited account with no failed attempts and no sign-in. What storage does
    // hold is each secret's engine-managed sealed column, and for the Administrator it is unset
    // (`Null`) until the sign-in service sets a password through the secrets path.
    assert_eq!(
        admin.data.fields[&n("user_name")],
        Value::from("administrator")
    );
    assert_eq!(admin.data.fields[&n("state")], Value::from("invited"));
    assert_eq!(admin.data.fields[&n("last_sign_in")], Value::Null);
    for secret in ["password_hash", "totp_secret"] {
        assert!(!admin.data.fields.contains_key(&n(secret)), "{secret}");
        assert_eq!(
            admin.data.fields[&n(&format!("{secret}__sealed"))],
            Value::Null,
            "{secret} is not set until the sign-in service sets it"
        );
        assert!(
            !admin
                .data
                .fields
                .contains_key(&n(&format!("{secret}__bidx"))),
            "{secret} is not a lookup field"
        );
    }
    // No column of the stored user holds anything that looks like a credential.
    for (field, value) in &admin.data.fields {
        if let Value::Bytes(bytes) = value {
            panic!("`{field}` holds {} bytes", bytes.len());
        }
    }
    let mut names: Vec<String> = column(&mut tx, doctype::ROLE, "role_name")
        .await
        .into_iter()
        .filter_map(|v| match v {
            Value::Text(t) => Some(t),
            _ => None,
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            role::SYSTEM_MANAGER,
            role::TENANT_ADMINISTRATOR,
            role::USER_MANAGER,
            role::WEBHOOK_MANAGER
        ]
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn a_later_edit_survives_a_second_seed() {
    let (backend, catalog) = installed().await;
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let outcome = seed(&mut tx, &SeedParams::default()).await.unwrap();
    let mut doc = tx
        .load(&n(doctype::USER), outcome.administrator, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    doc.data.fields.insert(n("state"), Value::from("active"));
    tx.update(&doc.data, doc.version).await.unwrap();
    seed(&mut tx, &SeedParams::default()).await.unwrap();
    let after = tx
        .load(&n(doctype::USER), outcome.administrator, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.data.fields[&n("state")], Value::from("active"));
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn seed_fails_cleanly_when_the_doctypes_are_absent() {
    let backend = FakeBackend::new();
    let empty = Arc::new(Catalog::new(1, collation(), vec![]).unwrap());
    backend.prepare(&empty).await.unwrap();
    let mut tx = backend.begin(empty, options()).await.unwrap();
    let err = seed(&mut tx, &SeedParams::default()).await.unwrap_err();
    assert!(matches!(err, SeedError::DoctypeMissing { .. }), "{err}");
    assert!(err.to_string().contains("not installed"), "{err}");
    tx.rollback().await.unwrap();
}

#[test]
fn the_bootstrap_data_holds_no_secret() {
    let text = include_str!("../../../../apps/platform/bootstrap.toml");
    let lower = text.to_lowercase();
    for word in ["password", "digest", "token", "totp", "secret ="] {
        // Comments may say what is absent; no key or value may carry one.
        for line in lower.lines().filter(|l| !l.trim_start().starts_with('#')) {
            assert!(!line.contains(word), "`{word}` in `{line}`");
        }
    }
    // Every field the seed sets exists and every definition it needs is embedded.
    assert!(definitions().is_ok());
}

#[test]
fn the_lookup_secrets_are_the_ones_found_by_value() {
    // Session and API Key credentials are found by their digest (WP-SEC-03.T3); passwords and
    // TOTP secrets are not.
    let defs = spot_platform_app::definitions().unwrap();
    let lookup = |doctype: &str, field: &str| {
        defs.iter()
            .find(|d| d.name == doctype)
            .and_then(|d| d.fields.iter().find(|f| f.name == field))
            .map(|f| f.lookup_index())
    };
    assert_eq!(lookup("session", "token_digest"), Some(true));
    assert_eq!(lookup("api_key", "digest"), Some(true));
    assert_eq!(lookup("user", "password_hash"), Some(false));
    assert_eq!(lookup("user", "totp_secret"), Some(false));
}
