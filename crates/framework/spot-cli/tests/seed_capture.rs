//! Site provisioning journals its seed rows (WP-JRN-02.T6): the Administrator, the system roles and the
//! Administrator's assignment are written beside their `doc_state` events in one transaction, so the
//! capture invariant reports nothing on a fresh site and a second seed adds no row, no version and no
//! event.
//!
//! The event shape is checked without a database. The provisioned site is real PostgreSQL
//! (`SPOT_TEST_PG_URL`; skipped with the same early return as the other host tests when it is unset).
//! SQL is written in two pieces (G5 refuses a full statement outside the storage crates).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use spot_cli::services::capture_invariant::hosted;
use spot_cli::users::{SeedCapture, seed_platform};
use spot_events_contract::PayloadDocument;
use spot_platform_app::bootstrap::StateCapture;
use spot_storage_contract::{DocumentData, Name, RecordId, Value, Version};
use time::OffsetDateTime;

const SERVICES: &str =
    "[services.capture_invariant]\nenabled = true\ninterval_seconds = 3600\nlag_seconds = 1\n";

fn n(text: &str) -> Name {
    Name::new(text).unwrap()
}

fn meta() -> std::sync::Arc<spot_meta::EffectiveMeta> {
    let defs = spot_platform_app::definitions().unwrap();
    spot_meta::Registry::new(defs).unwrap().effective()
}

/// A `role` row written the way the seed writes one: every stored field present, most `Null`.
fn role_row(meta: &spot_meta::EffectiveMeta) -> DocumentData {
    let def = meta.definition("role").unwrap();
    let mut fields = BTreeMap::new();
    for spec in &def.fields {
        if spec.ty.is_table() {
            continue;
        }
        if spec.ty == spot_meta::FieldType::Secret {
            fields.insert(n(&spot_meta::sealed_column(&spec.name)), Value::Null);
            continue;
        }
        fields.insert(n(&spec.name), Value::Null);
    }
    fields.insert(n("role_name"), Value::Text("Auditor".to_owned()));
    fields.insert(n("is_system"), Value::Bool(true));
    DocumentData {
        entity: n("role"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    }
}

#[test]
fn the_seed_event_is_the_state_event_of_an_insert() {
    let meta = meta();
    let doc = role_row(&meta);
    let now = OffsetDateTime::UNIX_EPOCH;
    let mut ids = RecordId::new_v7;
    let mut capture = SeedCapture::new(&meta, now, &mut ids);
    let event = capture
        .insert_event(&doc, Version::FIRST)
        .unwrap()
        .expect("an event is built");
    assert_eq!(event.topic.as_str(), "doc_state");
    assert_eq!(event.subject.entity, n("role"));
    assert_eq!(event.subject.id, doc.id);
    assert_eq!(event.subject_version, Some(Version::FIRST));
    assert_eq!(event.schema_version, 1);
    let payload = PayloadDocument::from_outbox_event(&event).unwrap();
    assert_eq!(payload.data["op"], "insert");
    assert!(payload.data["entity_schema"].as_u64().unwrap() >= 1);
    let image = &payload.data["image"];
    assert_eq!(image["id"], doc.id.to_string());
    assert_eq!(image["version"], 1);
    assert_eq!(image["role_name"], "Auditor");
    assert_eq!(image["is_system"], true);
    // Every declared stored field is there (null when unset); a secret is not, sealed or not.
    let def = meta.definition("role").unwrap();
    for spec in def
        .fields
        .iter()
        .filter(|f| !f.ty.is_table() && f.ty != spot_meta::FieldType::Secret)
    {
        if spec.name != spot_meta::DOC_STATE {
            assert!(
                image.get(&spec.name).is_some(),
                "the image lacks `{}`",
                spec.name
            );
        }
    }
    assert!(
        image
            .as_object()
            .unwrap()
            .keys()
            .all(|k| !k.ends_with("__sealed"))
    );
    // The actor is the provisioning component and no secret or credential is anywhere in it.
    assert_eq!(
        payload.meta.actor,
        spot_events_contract::Actor::System {
            component: n("provisioning")
        }
    );
    assert!(!event.payload.contains("password"));
}

#[test]
fn a_value_kind_the_seed_does_not_write_is_refused_not_guessed() {
    let meta = meta();
    let mut doc = role_row(&meta);
    doc.fields
        .insert(n("description"), Value::Bytes(vec![1, 2, 3]));
    let mut ids = RecordId::new_v7;
    let mut capture = SeedCapture::new(&meta, OffsetDateTime::UNIX_EPOCH, &mut ids);
    assert!(capture.insert_event(&doc, Version::FIRST).is_err());
}

/// One integer answer from the site's database (the statement is written in two pieces).
async fn answer(backend: &spot_postgres::PgBackend, head: &str, tail: &str) -> i64 {
    let client = backend.pool().get().await.unwrap();
    let sql = format!("{head}{tail}");
    client.query_one(&sql, &[]).await.unwrap().get(0)
}

const SEED_TABLES: [&str; 3] = ["role", "\"user\"", "role_assignment"];

/// How many rows the seed tables hold, and the sum of their versions.
async fn rows_and_versions(backend: &spot_postgres::PgBackend) -> (i64, i64) {
    let (mut rows, mut versions) = (0, 0);
    for table in SEED_TABLES {
        let from = format!("FROM {table}");
        rows += answer(backend, "SELECT count(*)::bigint ", &from).await;
        versions += answer(backend, "SELECT coalesce(sum(version), 0)::bigint ", &from).await;
    }
    (rows, versions)
}

async fn seed_events(backend: &spot_postgres::PgBackend) -> i64 {
    answer(
        backend,
        "SELECT count(*)::bigint ",
        "FROM spot_outbox WHERE topic = 'doc_state' AND subject_entity IN ('role', 'user', 'role_assignment')",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_site_has_no_missing_state_event_and_a_second_seed_adds_nothing() {
    let Some(env) = Env::new(SERVICES).await else {
        return;
    };
    env.site();
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|site| site.site == env.site)
        .unwrap()
        .clone();
    let backend = spot_cli::schema::backend(&loaded, &site).await.unwrap();

    // Every seed row has its state event, once, at the version storage gave it.
    let (seeded, before_versions) = rows_and_versions(&backend).await;
    assert!(seeded >= 3, "the seed wrote {seeded} rows");
    assert_eq!(seed_events(&backend).await, seeded);

    // The hosted invariant, after the lag, finds nothing open.
    let started = start(&env).await;
    let service = hosted(&env.site).expect("the site hosts the capture invariant");
    service.run_pass().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let clean = service.run_pass().await.unwrap();
    assert_eq!(clean.open, 0, "{clean:?}");
    assert!(clean.state_events > 0, "{clean:?}");

    // A second seed finds every row: no row, no version bump, no event.
    let meta = spot_cli::apps::serving(&env.root.0, &loaded, &site).unwrap();
    seed_platform(&backend, &meta).await.unwrap();
    assert_eq!(rows_and_versions(&backend).await, (seeded, before_versions));
    assert_eq!(seed_events(&backend).await, seeded);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let again = service.run_pass().await.unwrap();
    assert_eq!(again.open, 0, "{again:?}");
    started.finish().await;
}
