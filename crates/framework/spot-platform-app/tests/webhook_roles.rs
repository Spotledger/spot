//! The `Webhook Manager` role is a seeded system role (WP-INT-01.T8), and every role the shipped
//! grants name exists on a new site.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use common::{collation, meta};
use spot_platform_app::bootstrap::{SeedParams, seed};
use spot_platform_app::{doctype, role};
use spot_storage_contract::{
    Expr, FieldRef, Isolation, Name, Query, QueryExecutor, QueryPath, Selection, StorageBackend,
    Transaction, TxOptions, Value,
};
use spot_storage_fake::FakeBackend;

/// Roles a grants file may name without `bootstrap.toml` seeding them. Empty on purpose: every
/// role the platform grants to must exist on a new site. Add a name here only with a reason.
const NOT_SEEDED: &[(&str, &str)] = &[];

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn platform_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../apps/platform")
}

/// The role names of the `[[roles]]` rows in `file` (bootstrap) or of the `role` key of every
/// `[[entries]]` row (grants).
fn names_in(file: &PathBuf, table: &str, key: &str) -> BTreeSet<String> {
    let text = fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    let value: toml::Value = toml::from_str(&text).unwrap();
    value
        .get(table)
        .and_then(toml::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.get(key).and_then(toml::Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn every_role_named_in_a_grants_file_is_seeded() {
    let dir = platform_dir();
    let seeded = names_in(&dir.join("bootstrap.toml"), "roles", "name");
    let permissions = dir.join("permissions");
    let mut files = 0;
    for entry in fs::read_dir(&permissions).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with("grants.toml") {
            continue;
        }
        files += 1;
        // `grants.toml` and `webhook_grants.toml` use `[[entries]]`; `field_grants.toml` too.
        for role in names_in(&path, "entries", "role") {
            assert!(
                seeded.contains(&role) || NOT_SEEDED.iter().any(|(r, _)| *r == role),
                "`{name}` names role `{role}`, which bootstrap.toml does not seed"
            );
        }
    }
    assert!(files >= 3, "found {files} grants files");
    for (role, reason) in NOT_SEEDED {
        assert!(!reason.is_empty(), "`{role}` needs a reason");
    }
}

#[test]
fn webhook_manager_is_seeded_with_a_distinct_fixed_id() {
    let text = fs::read_to_string(platform_dir().join("bootstrap.toml")).unwrap();
    let value: toml::Value = toml::from_str(&text).unwrap();
    let roles = value["roles"].as_array().unwrap();
    assert!(
        roles
            .iter()
            .any(|r| r["name"].as_str() == Some(role::WEBHOOK_MANAGER))
    );
    let ids: BTreeSet<&str> = roles.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), roles.len(), "a role id is used twice");
    // The role is assignable like any other; the Administrator keeps only its own role.
    assert_eq!(
        value["administrator"]["role"].as_str(),
        Some(role::TENANT_ADMINISTRATOR)
    );
}

#[tokio::test]
async fn seeding_twice_writes_the_webhook_manager_once() {
    let catalog = Arc::new(meta().catalog(collation()).unwrap());
    let backend = FakeBackend::new();
    backend.prepare(&catalog).await.unwrap();
    let options = || TxOptions::new(n("webhook_roles"), Isolation::ReadCommitted);
    for _ in 0..2 {
        let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
        seed(&mut tx, &SeedParams::default()).await.unwrap();
        tx.commit().await.unwrap();
    }
    let mut tx = backend.begin(catalog.clone(), options()).await.unwrap();
    let alias = n("t");
    let mut q = Query::from(&n(doctype::ROLE), &alias, QueryPath::Report);
    q.select = vec![Selection {
        expr: Expr::Field(FieldRef {
            alias,
            field: n("role_name"),
        }),
        alias: n("v"),
    }];
    let rows = tx.query(&q).await.unwrap();
    let names: Vec<Value> = rows.rows.into_iter().flatten().collect();
    tx.rollback().await.unwrap();
    let webhook = names
        .iter()
        .filter(|v| **v == Value::from(role::WEBHOOK_MANAGER))
        .count();
    assert_eq!(webhook, 1, "{names:?}");
}
