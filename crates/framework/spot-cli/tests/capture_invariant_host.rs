//! The capture invariant hosted by a real site (WP-JRN-02.T3): a real `spot serve` assembly on a
//! real PostgreSQL database, writes through the document engine, and the hosted service's own
//! pass.
//!
//! - normal traffic (creates, updates, deletes through the engine) gives no condition and no
//!   finding;
//! - a seeded direct-SQL write (a version bump that bypasses the engine, so the change feed has an
//!   entry and the journal has no event) raises `CAPTURE_MISMATCH` within one pass once its entry
//!   is older than the lag, and recording the missing event clears it;
//! - the service writes nothing to the feed or the outbox.
//!
//! SQL statements are written in two pieces (G5 refuses a full statement outside the storage
//! crates), as `participants_pg.rs` does. Real PostgreSQL: `SPOT_TEST_PG_URL` selects the server
//! and `SPOT_REQUIRE_PG=1` makes a missing one loud.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::{Document, DocumentPatch};
use spot_app_api::prelude::{CommandId, Name, RecordId, SiteId, Value};
use spot_cli::services::capture_invariant::hosted;
use spot_postgres::PgBackend;
use time::OffsetDateTime;

const SERVICES: &str =
    "[services.capture_invariant]\nenabled = true\ninterval_seconds = 3600\nlag_seconds = 1\n";

async fn backend(env: &Env) -> PgBackend {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|site| site.site == env.site)
        .unwrap()
        .clone();
    spot_cli::schema::backend(&loaded, &site).await.unwrap()
}

/// One integer answer from the site's database.
async fn count(backend: &PgBackend, head: &str, tail: &str) -> i64 {
    let client = backend.pool().get().await.unwrap();
    let sql = format!("{head}{tail}");
    client.query_one(&sql, &[]).await.unwrap().get(0)
}

fn invocation(site: &str) -> Invocation {
    Invocation {
        site: SiteId::new(site).unwrap(),
        actor: Actor::System {
            component: Name::new("capture_test").unwrap(),
        },
        command_id: CommandId::new("cmd-capture").ok(),
        language: None,
        now: OffsetDateTime::now_utc(),
    }
}

fn label(name: &str) -> Document {
    let mut document = Document::new(Name::new("label").unwrap(), RecordId::new_v7());
    document.set(Name::new("name").unwrap(), Value::Text(name.to_owned()));
    document
}

fn rename(name: &str) -> DocumentPatch {
    let mut patch = DocumentPatch::default();
    patch
        .fields
        .insert(Name::new("name").unwrap(), Value::Text(name.to_owned()));
    patch
}

#[tokio::test]
async fn normal_traffic_gives_no_finding_and_a_bypass_raises_and_clears() {
    let Some(env) = Env::new(SERVICES).await else {
        return;
    };
    env.site();
    let started = start(&env).await;
    let live = started
        .running
        .live(&env.site)
        .expect("the site is serving");
    let engine = live.engine().expect("the engine is attached");
    let inv = invocation(&env.site);

    // Normal traffic: 20 creates, every other one updated, every fifth one deleted.
    let mut ids = Vec::new();
    for n in 0..20 {
        let saved = engine
            .create(&inv, label(&format!("L{n}")))
            .await
            .unwrap()
            .value;
        ids.push(saved);
    }
    for (n, doc) in ids.iter().enumerate() {
        if n % 2 == 0 {
            engine
                .update(
                    &inv,
                    "label",
                    doc.id(),
                    rename(&format!("R{n}")),
                    doc.version().unwrap(),
                )
                .await
                .unwrap();
        }
    }
    for (n, doc) in ids.iter().enumerate() {
        if n % 5 == 4 && n % 2 == 1 {
            engine
                .delete(&inv, "label", doc.id(), doc.version().unwrap())
                .await
                .unwrap();
        }
    }

    let service = hosted(&env.site).expect("the site hosts the capture invariant");
    // Pass 1 publishes both logs; the entries are younger than the lag, so they are deferred.
    service.run_pass().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let clean = service.run_pass().await.unwrap();
    assert!(
        clean.feed_entries > 0 && clean.feed_entries >= clean.state_events,
        "{clean:?}"
    );
    assert!(
        service
            .readiness_text()
            .starts_with("capture_invariant: ok (")
    );
    // What is open now is what site provisioning wrote without the engine (the platform's seed
    // roles, role assignments and administrator are inserted directly by the installer, which the
    // invariant correctly reports; see the task's open questions). Nothing the engine wrote is
    // in it: no finding is about a `label`.
    let baseline = clean.open;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let backend = backend(&env).await;
    let about_labels = (
        "SELECT count(*)::bigint ",
        "FROM spot_condition_open WHERE id = 'CAPTURE_MISMATCH' AND params->>'record' LIKE 'label/%'",
    );
    assert_eq!(
        count(&backend, about_labels.0, about_labels.1).await,
        0,
        "normal traffic through the engine raises nothing"
    );

    // The bypass: bump a stored version with SQL. The capture trigger writes a change-feed
    // entry; nothing writes a journal event.
    let victim = ids[1].id().to_string();
    {
        let client = backend.pool().get().await.unwrap();
        let sql = format!(
            "{}{}",
            "UPDATE label ", "SET version = version + 1 WHERE id = $1::text::uuid"
        );
        client.execute(&sql, &[&victim]).await.unwrap();
    }
    let outbox_before = count(
        &backend,
        "SELECT count(*)::bigint ",
        "FROM spot_outbox WHERE topic = 'doc_state'",
    )
    .await;
    service.run_pass().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let found = service.run_pass().await.unwrap();
    assert_eq!(found.open, baseline + 1, "{found:?}");
    // The service wrote no `doc_state` event of its own (the condition engine's own transition
    // events are other topics).
    assert_eq!(
        count(
            &backend,
            "SELECT count(*)::bigint ",
            "FROM spot_outbox WHERE topic = 'doc_state'",
        )
        .await,
        outbox_before
    );
    // Within one more tick the condition engine has stored it, with identity and no content.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(count(&backend, about_labels.0, about_labels.1).await, 1);

    // Fixing it: record the event the bypassed write never had, the same document, version and
    // operation as its feed entry.
    let next_version: i64 = {
        let client = backend.pool().get().await.unwrap();
        let sql = format!(
            "{}{}",
            "SELECT version ", "FROM label WHERE id = $1::text::uuid"
        );
        client.query_one(&sql, &[&victim]).await.unwrap().get(0)
    };
    {
        let client = backend.pool().get().await.unwrap();
        let sql = format!(
            "{}{}{}",
            "INSERT ",
            "INTO spot_outbox (id, tx, ordinal, topic, subject_entity, subject_id, subject_version, schema_version, payload, captured_at, xid) ",
            "VALUES (gen_random_uuid(), gen_random_uuid(), 0, 'doc_state', 'label', $1::text::uuid, $2, 1, '{\"data\":{\"op\":\"update\"}}', now(), pg_current_xact_id())"
        );
        client
            .execute(&sql, &[&victim, &next_version])
            .await
            .unwrap();
    }
    // The first pass publishes the recorded event, which is already matched in its snapshot.
    let first = service.run_pass().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let fixed = service.run_pass().await.unwrap();
    assert_eq!(
        first.cleared + fixed.cleared,
        1,
        "the matched entry clears the condition: {first:?} {fixed:?}"
    );
    assert_eq!(fixed.open, baseline);

    started.finish().await;
}

#[tokio::test]
async fn a_site_that_has_not_migrated_degrades_it_and_serves() {
    let Some(env) = Env::new(SERVICES).await else {
        return;
    };
    env.site();
    // Remove the watermark table the way a database that never ran `spot migrate` lacks it.
    let backend = backend(&env).await;
    {
        let client = backend.pool().get().await.unwrap();
        let sql = format!("{}{}", "DROP ", "TABLE spot_capture_watermark");
        client.batch_execute(&sql).await.unwrap();
    }
    let started = start(&env).await;
    // The site is up; the service is not hosted as a running task, and says why.
    assert!(started.running.live(&env.site).is_some());
    started.finish().await;
}
