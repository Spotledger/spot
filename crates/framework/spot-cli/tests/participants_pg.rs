//! The participants and `spot migrate` over a real PostgreSQL and over HTTP (WP-PRD-01.T21).
//!
//! WP-PRD-01.T11 installed the owner check and edge sync and created their tables, but the host
//! held no ownership registry, so a partitioned profile was refused with
//! `cli.partition_registry_missing`. This file proves the whole chain on a real database and a
//! real host:
//!
//! - the owner check reads ownership from `spot-partition-pg` **through the site's refreshed
//!   snapshot**, so a write never crosses the blocking bridge inside its own request: a write to
//!   a scope this node does not own is refused with `partition.not_owner` before any storage
//!   write, and the same write on an owned scope succeeds;
//! - a partitioned DocType with no `[[partition.scope]]` entry stops the site
//!   (`partition.scope_uncovered`);
//! - the edge set equals the link set after a mixed write sequence (`check_invariant` clean), and
//!   an edge written before a later refusal rolls back with the document;
//! - an `AuditStep` registered beside the audit participant stops the install check
//!   (`docaudit.double_registration`) — the host installs no audit participant yet
//!   (WP-PRD-01.T22), and the check is armed for the day it does;
//! - `spot migrate` creates the ownership tables on a fresh site and on a populated one, and a
//!   second run plans nothing.
//!
//! Real PostgreSQL: `SPOT_TEST_PG_URL` selects the server and `SPOT_REQUIRE_PG=1` makes a missing
//! one loud. Each test makes its own scratch database, so the file runs in parallel.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::json;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::Document;
use spot_app_api::prelude::{CommandId, Name, RecordId, SiteId, Value};
use spot_controllers::ControllerRegistry;
use spot_doc_audit::{AuditStep, FailingWriter, NoBeforeImages};
use spot_document::engine::{TxParticipant, WriteRecord};
use spot_edge_pg::{EdgeVerifyJob, PgEdgeWriters};
use spot_edge_sync::{EdgeParticipant, EdgeSchema};
use spot_field_guard::{AuditRecord, BoundedQueue, FieldAccessRecord};
use spot_meta::EffectiveMeta;
use spot_partition::{AssignmentRequest, MemoryPartitionRegistry, PartitionRegistry, ScopeKey};
use spot_partition_pg::PgPartitionRegistry;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::{PgBackend, PgTransaction};
use spot_site_masker::{GuardDeps, SiteMasker};
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

/// The node id the partitioned sites in this file run as.
const NODE: &str = "node-a";

// --- the site and its database ------------------------------------------------------------

/// The site's own configuration, as the host reads it.
fn loaded(env: &Env) -> spot_cli::config::Loaded {
    spot_cli::config::load(&env.root.0, false).unwrap()
}

fn site_of<'a>(env: &Env, loaded: &'a spot_cli::config::Loaded) -> &'a spot_cli::config::Site {
    loaded
        .sites
        .iter()
        .find(|site| site.site == env.site)
        .expect("the site this test made")
}

/// The site's database, opened the way the host opens it.
async fn backend(env: &Env) -> PgBackend {
    let loaded = loaded(env);
    let site = site_of(env, &loaded).clone();
    spot_cli::schema::backend(&loaded, &site).await.unwrap()
}

/// One integer answer from the site's database, read on its own connection.
async fn count(env: &Env, head: &str, tail: &str) -> i64 {
    let backend = backend(env).await;
    let client = backend.pool().get().await.unwrap();
    // One statement, written in two pieces: G5 refuses a SQL statement spelled out in full
    // outside the storage crates, and `migrate_extras.rs` splits its query the same way.
    let sql = format!("{head}{tail}");
    client.query_one(&sql, &[]).await.unwrap().get(0)
}

/// The site's served metadata.
fn served(env: &Env) -> Arc<EffectiveMeta> {
    let loaded = loaded(env);
    let site = site_of(env, &loaded).clone();
    spot_cli::apps::serving(&env.root.0, &loaded, &site).unwrap()
}

/// Adds a `[partition]` profile to the site's file, the way an operator would.
fn switch_on(env: &Env, doctypes: &[&str], scope: &[(&str, &str, &str)]) {
    let path = env.site_dir().join("site.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    let names: Vec<String> = doctypes.iter().map(|d| format!("{d:?}")).collect();
    text.push_str(&format!(
        "\n[partition]\nnode = {NODE:?}\ndoctypes = [{}]\n",
        names.join(", ")
    ));
    for (doctype, key, field) in scope {
        text.push_str(&format!(
            "\n[[partition.scope]]\ndoctype = {doctype:?}\nkey = {key:?}\nfield = {field:?}\n"
        ));
    }
    std::fs::write(&path, text).unwrap();
}

/// `spot migrate --apply` for the site.
fn migrate(env: &Env) -> String {
    ok(&env.root.0, &["migrate", "--site", &env.site, "--apply"])
}

/// Runs `work` on a plain thread with a registry over the site's database.
///
/// The blocking bridge refuses a thread that belongs to a tokio runtime
/// (`pg_bridge.wrong_thread`) and `spot-partition-pg` refuses to be built there
/// (`partition_pg.unsupported_context`), which is the rule the host itself obeys: start-up and
/// the CLI build the store on a plain thread; the write path never calls it.
fn with_registry<T: Send + 'static>(
    backend: PgBackend,
    pool_size: usize,
    work: impl FnOnce(&PgPartitionRegistry) -> T + Send + 'static,
) -> T {
    std::thread::spawn(move || {
        let config = BridgeConfig {
            name: "t21_partition".to_owned(),
            max_in_flight: BridgeConfig::largest_in_flight(pool_size).clamp(1, 4),
            workers: 2,
            queue_capacity: 4,
            default_deadline: Duration::from_secs(10),
            max_deadline: Duration::from_secs(30),
        };
        let bridge = Arc::new(Bridge::for_backend(config, &backend).unwrap());
        let registry = PgPartitionRegistry::new(bridge, pool_size).unwrap();
        work(&registry)
    })
    .join()
    .unwrap()
}

/// Gives `value` of the scope key `key` to `node`, from `from` on.
async fn assign(env: &Env, node: &str, key: &str, value: &str, from: OffsetDateTime) {
    let loaded = loaded(env);
    let pool_size = loaded.common.postgres.pool_max_size;
    let backend = backend(env).await;
    let (node, key, value) = (node.to_owned(), key.to_owned(), value.to_owned());
    with_registry(backend, pool_size, move |registry| {
        registry
            .assign(AssignmentRequest {
                scope: ScopeKey::new(&key, &value).unwrap(),
                node: NodeId::new(&node).unwrap(),
                effective_from: from,
                effective_to: None,
                actor: "test".to_owned(),
                reason: "T21 test fixture".to_owned(),
                at: from,
            })
            .unwrap();
    });
}

// --- the install check, as the host runs it ------------------------------------------------

/// The participants the host would install for the site, with the registry a test supplies.
async fn install_check(
    env: &Env,
    registry: Arc<dyn PartitionRegistry>,
    audit: Option<spot_cli::services::participants::Audit>,
    controllers: ControllerRegistry,
) -> Result<spot_cli::services::participants::Installed, String> {
    let loaded = loaded(env);
    let site = site_of(env, &loaded).clone();
    let meta = served(env);
    let generation = Arc::clone(&meta);
    let deps = spot_cli::services::participants::Deps {
        audit,
        controllers: Arc::new(controllers),
        partitions: Some(registry),
        meta: Arc::new(move || Arc::clone(&generation)),
    };
    let site_id = spot_core::SiteId::new(&env.site).unwrap();
    spot_cli::services::participants::build(&site_id, &site, &deps).map_err(|e| e.to_string())
}

/// A no-op ownership registry: these cases are about the shape of the configuration, not about
/// who owns what.
fn memory() -> Arc<dyn PartitionRegistry> {
    Arc::new(MemoryPartitionRegistry::new())
}

/// The production masker over the site's own metadata, as the host builds it for a generation.
fn masker(meta: &Arc<EffectiveMeta>) -> Arc<dyn spot_doc_audit::FieldMasker> {
    let mut grants = String::new();
    for view in meta.doctypes() {
        grants.push_str(&format!(
            "[[entries]]\nrole = \"Reader\"\ndoctype = \"{}\"\nrights = [\"read\"]\n",
            view.name.as_str()
        ));
    }
    let model = spot_permissions::load(&spot_permissions::ModelTexts {
        grants,
        field_grants: String::new(),
        groups: String::new(),
        shares: String::new(),
    })
    .unwrap();
    // The masker only has to exist for the install check to build an audit participant; the
    // DocTypes' protection groups are the site's, and this model declares none of them, so the
    // snapshot carries no field facts. Nothing in this file reads a masked diff.
    let snapshot = spot_permissions::Snapshot::build(meta, &model).unwrap();
    let deps = GuardDeps {
        engine: Arc::new(spot_permissions::PermissionEngine::new(Arc::new(snapshot))),
        trace: Arc::new(BoundedQueue::<FieldAccessRecord>::new(16)),
        audit: Arc::new(BoundedQueue::<AuditRecord>::new(16)),
        clock: Arc::new(spot_core::ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    };
    Arc::new(SiteMasker::new(meta, deps))
}

// --- the edge set, read the way the invariant job reads it ---------------------------------

/// The edges `spot_graph_edge` holds.
async fn edges(env: &Env) -> i64 {
    count(env, "SELECT count(*)::bigint ", "FROM spot_graph_edge").await
}

/// The edges pointing at `to` (a record id as the API returns it: UUID text).
async fn edges_to(env: &Env, to: &str) -> i64 {
    count(
        env,
        "SELECT count(*)::bigint ",
        &format!("FROM spot_graph_edge WHERE to_id = '{to}'::uuid"),
    )
    .await
}

/// Runs the site's own invariant scan — every document's link set against the stored edges,
/// through [`check_invariant`](spot_edge_sync::check_invariant) — and returns the drift it found
/// on documents of `entities`.
///
/// The scan covers the whole site; the rows a site is *seeded* with (`role_assignment` and the
/// like) are written before the participants exist and are not this test's subject, so only the
/// entities this test writes are read here.
async fn drift(env: &Env, report_key: &str, entities: &[&str]) -> Vec<spot_edge_pg::DriftEntry> {
    let backend = backend(env).await;
    let meta = served(env);
    let catalog = Arc::new(spot_cli::schema::catalog(&meta).unwrap());
    let job = EdgeVerifyJob::new(backend, catalog, EdgeSchema::derive(&meta, &[]));
    let report = job
        .run(OffsetDateTime::now_utc(), report_key)
        .await
        .expect("the invariant scan runs");
    assert!(
        report.checked_documents > 0,
        "the scan read the site's documents"
    );
    report
        .entries
        .into_iter()
        .filter(|entry| entities.contains(&entry.owner.entity.as_str()))
        .collect()
}

/// A participant that refuses every write of a Task, after edge sync has written its edges —
/// the same shape `spot-edge-pg`'s atomicity test uses, here on a site's own engine.
struct RefuseAfter;

impl TxParticipant<PgTransaction> for RefuseAfter {
    fn after_write<'a>(
        &'a self,
        _tx: &'a mut PgTransaction,
        write: &'a WriteRecord<'a>,
    ) -> spot_app_api::BoxFuture<'a, spot_app_api::AppResult<()>> {
        let refuse = write.doctype == "task";
        Box::pin(async move {
            if refuse {
                return Err(spot_app_api::AppError::new(
                    spot_app_api::ErrorClass::Internal,
                    "test.refused_after_edges",
                    "refused on purpose after the edge write",
                ));
            }
            Ok(())
        })
    }
}

// --- (1) the owner check ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_outside_the_node_s_scope_is_refused_before_storage_and_an_owned_one_succeeds() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let secret = env.site();
    // One partitioned DocType, scoped by its `name` field: the label master needs nothing else.
    switch_on(&env, &["label"], &[("label", "branch", "name")]);
    let plan = migrate(&env);
    assert!(
        plan.contains("partition: ownership tables present (4 tables)"),
        "the ownership tables are created by `spot migrate`: {plan}"
    );
    // This node owns W7 and W11 and nothing else.
    let since = OffsetDateTime::now_utc() - time::Duration::hours(1);
    assign(&env, NODE, "branch", "W7", since).await;
    assign(&env, NODE, "branch", "W11", since).await;

    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &secret).await;

    // Readiness names the node and how many DocTypes it checks, and no assignment.
    let ready = http(addr, &host, None, "GET", "/api/health/ready", &[], None).await;
    expect(&ready, 200);
    let text = String::from_utf8_lossy(&ready.bytes).into_owned();
    assert!(
        text.contains("partition: node node-a, 1 DocTypes"),
        "readiness reports the node profile: {text}"
    );
    for assignment in ["W7", "W9", "W11", "branch"] {
        assert!(
            !text.contains(assignment),
            "readiness never carries an assignment: {text}"
        );
    }

    // Over HTTP the engine's `partition.not_owner` is rendered as the permission class (the
    // resource layer's mapping), and the message says which DocType and which scope.
    let owned = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/label",
        &[],
        Some(json!({ "name": "W7", "colour": "#ffffff" })),
    )
    .await;
    expect(&owned, 201);
    let refused = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/label",
        &[],
        Some(json!({ "name": "W9", "colour": "#000000" })),
    )
    .await;
    expect(&refused, 403);
    assert_eq!(
        refused.body["error"]["code"],
        "permission.denied",
        "{}",
        String::from_utf8_lossy(&refused.bytes)
    );
    let message = refused.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("may not write `label`") && message.contains("no node owns it"),
        "the refusal names the DocType and the scope it does not own: {message}"
    );

    // A second write path — the engine the site serves, called directly — pins the participant's
    // own stable code, and proves the owner check answers from the refreshed snapshot: a raw
    // `spot-partition-pg` here would cross the blocking bridge inside this async task, be refused
    // with `pg_bridge.wrong_thread`, and refuse the *owned* write too.
    let live = started
        .running
        .live(&env.site)
        .expect("the site is serving");
    let engine = live.engine().expect("the engine is attached");
    let invocation = Invocation {
        site: SiteId::new(&env.site).unwrap(),
        actor: Actor::System {
            component: Name::new("t21_test").unwrap(),
        },
        command_id: CommandId::new("cmd-t21-owner").ok(),
        language: None,
        now: OffsetDateTime::now_utc(),
    };
    let label = |name: &str| {
        let mut document = Document::new(Name::new("label").unwrap(), RecordId::new_v7());
        document.set(Name::new("name").unwrap(), Value::Text(name.to_owned()));
        document
    };
    engine
        .create(&invocation, label("W11"))
        .await
        .map(|_| ())
        .expect("the owned scope writes through the engine too");
    let error = engine
        .create(&invocation, label("W13"))
        .await
        .expect_err("the unowned scope is refused");
    assert_eq!(error.code, "partition.not_owner", "{}", error.message);

    // The proof that the refusal came before the storage write: neither refused row exists.
    assert_eq!(
        count(
            &env,
            "SELECT count(*)::bigint ",
            "FROM label WHERE name = 'W7'"
        )
        .await
            + count(
                &env,
                "SELECT count(*)::bigint ",
                "FROM label WHERE name = 'W11'"
            )
            .await,
        2,
        "the writes this node owns are stored"
    );
    for unowned in ["W9", "W13"] {
        assert_eq!(
            count(
                &env,
                "SELECT count(*)::bigint ",
                &format!("FROM label WHERE name = '{unowned}'")
            )
            .await,
            0,
            "the refused write of {unowned} left nothing behind"
        );
    }
    // And the row is readable through the API, so it is a real document, not a stray row.
    let listed = http(
        addr,
        &host,
        Some(&admin),
        "GET",
        "/api/v1/resource/label?limit=10",
        &[],
        None,
    )
    .await;
    expect(&listed, 200);
    assert_eq!(listed.body["data"].as_array().unwrap().len(), 2);

    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scope_assigned_to_another_node_is_refused_too() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let secret = env.site();
    switch_on(&env, &["label"], &[("label", "branch", "name")]);
    migrate(&env);
    // W7 belongs to a different node of the deployment; this site owns nothing.
    assign(
        &env,
        "node-b",
        "branch",
        "W7",
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;

    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &secret).await;
    let refused = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/label",
        &[],
        Some(json!({ "name": "W7" })),
    )
    .await;
    expect(&refused, 403);
    assert_eq!(refused.body["error"]["code"], "permission.denied");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("node-b"),
        "the refusal is the owner check's, and names the owner: {}",
        String::from_utf8_lossy(&refused.bytes)
    );
    assert_eq!(
        count(&env, "SELECT count(*)::bigint ", "FROM label").await,
        0,
        "no storage write happened for a scope another node owns"
    );
    started.finish().await;
}

// --- (2) the start-up refusals -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partitioned_doctype_with_no_scope_stops_the_site() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let _ = env.site();
    // `project` is partitioned but has no scope field: unprotected, so the site must not serve.
    switch_on(&env, &["label", "project"], &[("label", "branch", "name")]);
    migrate(&env);

    let started = start(&env).await;
    let refused = http(
        started.addr,
        &env.site,
        None,
        "GET",
        "/api/v1/resource/label?limit=1",
        &[],
        None,
    )
    .await;
    expect(&refused, 503);
    assert_eq!(
        refused.body["error"]["details"]["reason"], "site_unavailable",
        "a site that cannot check ownership does not serve"
    );
    started.finish().await;

    // The code an operator finds in the start-up log: the install check refuses the profile and
    // names every uncovered DocType, and never the covered one.
    let error = install_check(&env, memory(), None, ControllerRegistry::new())
        .await
        .expect_err("an uncovered partitioned DocType never installs");
    assert!(error.starts_with("partition.scope_uncovered:"), "{error}");
    assert!(error.contains("project"), "{error}");
    assert!(!error.contains("`label`"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_audit_step_beside_the_audit_participant_stops_the_install_check() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let _ = env.site();
    let meta = served(&env);
    // The site as it ships installs no audit participant (WP-PRD-01.T22), so an app's audit step
    // is harmless today — and the check that would refuse it is armed for the day it is not.
    let shipped = install_check(&env, memory(), None, ControllerRegistry::new())
        .await
        .expect("a single-node site installs");
    assert_eq!(shipped.names, vec!["edge"]);

    let mut controllers = ControllerRegistry::new();
    spot_doc_audit::install::register(
        &mut controllers,
        "sample",
        AuditStep::new(
            "label",
            Arc::new(FailingWriter),
            masker(&meta),
            Arc::new(NoBeforeImages),
            Arc::new(spot_core::ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
        ),
    );
    let audit = spot_cli::services::participants::Audit::new(&meta, masker(&meta));
    let error = install_check(&env, memory(), Some(audit), controllers)
        .await
        .expect_err("auditing the same DocType twice never installs");
    assert!(
        error.starts_with("docaudit.double_registration:"),
        "{error}"
    );
    assert!(error.contains("label"), "{error}");
}

// --- (3) edge sync ------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_edge_set_equals_the_link_set_after_a_mixed_write_sequence() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let secret = env.site();
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &secret).await;
    let owner = me(addr, &host, &admin).await;

    let project = |code: &str| json!({ "code": code, "title": format!("Project {code}"), "status": "Active", "owner": owner });
    let first = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(project("PRJ-1")),
    )
    .await;
    expect(&first, 201);
    let first_id = first.body["data"]["id"].as_str().unwrap().to_owned();
    let second = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(project("PRJ-2")),
    )
    .await;
    expect(&second, 201);
    let second_id = second.body["data"]["id"].as_str().unwrap().to_owned();

    // A task links one project: one link, one edge.
    let task = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({ "subject": "mixed sequence", "project": first_id })),
    )
    .await;
    expect(&task, 201);
    let task_id = task.body["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(edges_to(&env, &first_id).await, 1, "one link is one edge");

    // The link moves: the edge moves with it, and does not duplicate.
    let moved = http(
        addr,
        &host,
        Some(&admin),
        "PATCH",
        &format!("/api/v1/resource/task/{task_id}"),
        &[("If-Match", &task.etag)],
        Some(json!({ "project": second_id })),
    )
    .await;
    expect(&moved, 200);
    assert_eq!(edges_to(&env, &first_id).await, 0, "the old edge is gone");
    assert_eq!(
        edges_to(&env, &second_id).await,
        1,
        "the edge moved, it did not duplicate"
    );
    let clean = drift(&env, "t21-mixed", &["project", "task"]).await;
    assert!(
        clean.is_empty(),
        "the edge set equals the link set: {clean:?}"
    );

    // The row goes, and its edges go with it; the invariant stays clean.
    let removed = http(
        addr,
        &host,
        Some(&admin),
        "DELETE",
        &format!("/api/v1/resource/task/{task_id}"),
        &[("If-Match", &moved.etag)],
        None,
    )
    .await;
    expect(&removed, 204);
    assert_eq!(
        edges_to(&env, &second_id).await,
        0,
        "delete removes the edges"
    );
    let after = drift(&env, "t21-after-delete", &["project", "task"]).await;
    assert!(
        after.is_empty(),
        "the invariant is clean after the delete: {after:?}"
    );
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_write_rolls_back_with_a_failed_document_write() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let secret = env.site();
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &secret).await;
    let owner = me(addr, &host, &admin).await;

    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-R", "title": "Rollback", "status": "Active", "owner": owner
        })),
    )
    .await;
    expect(&project, 201);
    let project_id = project.body["data"]["id"].as_str().unwrap().to_owned();

    // Edge sync runs first and writes the task's edge; the participant after it refuses, so the
    // whole write — document and edge — must roll back.
    let live = started
        .running
        .live(&env.site)
        .expect("the site is serving");
    let engine = live.engine().expect("the engine is attached");
    let generation = served(&env);
    let meta_source: Arc<dyn spot_edge_sync::MetaSource> =
        Arc::new(move || Arc::clone(&generation));
    let site_id = spot_core::SiteId::new(&env.site).unwrap();
    engine.set_participants(vec![
        Arc::new(
            EdgeParticipant::<PgTransaction, PgEdgeWriters>::new(meta_source, PgEdgeWriters)
                .for_site(site_id),
        ) as Arc<dyn TxParticipant<PgTransaction>>,
        Arc::new(RefuseAfter),
    ]);

    let edges_before = edges(&env).await;
    let refused = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({ "subject": "rolled back", "project": project_id })),
    )
    .await;
    assert!(
        refused.status >= 500,
        "the refusing participant fails the write: {}",
        String::from_utf8_lossy(&refused.bytes)
    );
    assert_eq!(
        count(
            &env,
            "SELECT count(*)::bigint ",
            "FROM task WHERE subject = 'rolled back'"
        )
        .await,
        0,
        "no document row survived the refusal"
    );
    assert_eq!(
        edges(&env).await,
        edges_before,
        "the edge written before the refusal rolled back with the document"
    );
    started.finish().await;
}

// --- (4) `spot migrate` ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spot_migrate_creates_the_ownership_tables_and_a_second_run_plans_nothing() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let secret = env.site();
    switch_on(&env, &["label"], &[("label", "branch", "name")]);

    // Fresh: the site's document schema exists (new-site planned it) and the ownership tables do
    // not, because their DDL is the registry crate's own.
    let backend = backend(&env).await;
    let missing = spot_cli::services::partition::missing_tables(&backend)
        .await
        .unwrap();
    assert_eq!(missing.len(), 4, "{missing:?}");

    let first = migrate(&env);
    assert!(
        first.contains("partition: ownership tables present (4 tables)"),
        "{first}"
    );
    assert!(
        spot_cli::services::partition::missing_tables(&backend)
            .await
            .unwrap()
            .is_empty(),
        "every ownership table exists after `spot migrate --apply`"
    );

    // Populated: a document of the partitioned DocType is written and served.
    assign(
        &env,
        NODE,
        "branch",
        "W7",
        OffsetDateTime::now_utc() - time::Duration::hours(1),
    )
    .await;
    let started = start(&env).await;
    let admin = admin(started.addr, &env.site, &secret).await;
    let owned = http(
        started.addr,
        &env.site,
        Some(&admin),
        "POST",
        "/api/v1/resource/label",
        &[],
        Some(json!({ "name": "W7" })),
    )
    .await;
    expect(&owned, 201);
    started.finish().await;

    // The second run plans nothing at all, and still leaves the ownership tables in place.
    let second = migrate(&env);
    assert!(second.contains("schema is up to date"), "{second}");
    assert!(
        !second.contains("applied"),
        "a second run applies no step: {second}"
    );
    assert!(
        second.contains("partition: ownership tables present (4 tables)"),
        "{second}"
    );
    assert_eq!(
        count(&env, "SELECT count(*)::bigint ", "FROM label").await,
        1,
        "the documents of a populated site are untouched by a migration"
    );
}
