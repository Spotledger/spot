//! Governance and process-first entry hosted over a real PostgreSQL ledger (WP-BPM-08.T3;
//! ADR-49, ADR-080).
//!
//! Every test needs a real PostgreSQL server (`SPOT_TEST_PG_URL`) and skips, printing
//! `SKIPPED:`, when it is unset (the convention of `tests/process_documents.rs`). The guard runs
//! against a real `PgTransaction`, exactly as the document engine hands it to a participant;
//! the parts that need a live site (the acting principal) are the seams of [`Wiring`], filled
//! with in-memory fakes. The HTTP side of governance is the document engine's: a refusal
//! raised here is the engine's error envelope unchanged, and the engine's own route tests prove
//! it reaches the wire; this file proves the guard's decision, its transaction behaviour and its
//! audit.
//!
//! What is covered: a direct create on a governed DocType refused with the stable code; the
//! same through a Run, consuming the grant; a replayed, forged, foreign, stale and
//! other-attempt grant; a client-chosen command id claiming a process origin; admin override
//! off and on (with its audit row); the outside-process permission and its reason floor; the
//! audit row of every refusal; `process_only` creates and changes; drafts invisible to others,
//! visible to the Run's participants once submitted, written in a transaction and reaped; the
//! guard failing closed when the policy or the key is unavailable; two-site isolation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use deadpool_postgres::Pool;
use serde_json::{Map, json};
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{CommandId, Name, RecordId, SiteId};
use spot_app_api::{BoxFuture, Document, Operation};
use spot_bpm_entry::{CreateRoute, DraftSave, DraftWriter, RequestContext};
use spot_bpm_governance::{
    GovernanceError, GovernanceMode, GovernancePolicy, OUTSIDE_PROCESS_PERMISSION, PolicyChange,
    PolicyStore, REASON_FIELD, RoleOutsideProcess,
};
use spot_bpm_model::DraftMode;
use spot_cli::services::process_governance::{
    DerivedSecrets, DraftError, DraftState, DraftStore, EntryProcessEntry, GovernanceHost,
    GovernedEntry, IssueError, PrincipalLookup, Settings, SitePolicy, Wiring,
};
use spot_core::{Clock, ManualClock};
use spot_document::engine::{WriteCause, WriteIntent};
use spot_permissions::Principal;
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{
    Catalog, Collation, Isolation, StorageBackend, Transaction, TxOptions,
};
use time::OffsetDateTime;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

fn start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// The stable codes, spelled out so a rename shows up here.
const DENIED: &str = "bpm_governance.outside_process_denied";
const REASON: &str = "bpm_governance.reason_required";
const SETTINGS_CODE: &str = "bpm_governance.settings";
const START_PROCESS: &str = "bpm_entry.start_process";

// ---------------------------------------------------------------------------------------------
// Fakes for the live-site seams.
// ---------------------------------------------------------------------------------------------

/// Roles by user id; an unknown user has none. A system actor is the system principal.
#[derive(Default)]
struct Roles(BTreeMap<String, Vec<String>>);

impl PrincipalLookup for Roles {
    fn principal<'a>(&'a self, actor: &'a Actor) -> BoxFuture<'a, Principal> {
        Box::pin(async move {
            match actor {
                Actor::System { .. } => Principal::system(),
                _ => {
                    let text = actor.principal_text();
                    let roles: Vec<&str> = self
                        .0
                        .get(&text)
                        .map(|r| r.iter().map(String::as_str).collect())
                        .unwrap_or_default();
                    Principal::with_roles(&text, &roles)
                }
            }
        })
    }
}

/// A policy that cannot be read.
struct BrokenPolicy;

fn down<T>() -> Result<T, GovernanceError> {
    Err(GovernanceError::Settings {
        detail: "down".to_owned(),
    })
}

impl PolicyStore for BrokenPolicy {
    fn mode_at(
        &self,
        _: &str,
        _: &str,
        _: OffsetDateTime,
    ) -> Result<GovernanceMode, GovernanceError> {
        down()
    }
    fn min_reason_chars(&self, _: &str) -> Result<usize, GovernanceError> {
        down()
    }
    fn current(&self, _: &str, _: &str) -> Result<GovernancePolicy, GovernanceError> {
        down()
    }
    fn changes(&self, _: &str, _: &str) -> Result<Vec<PolicyChange>, GovernanceError> {
        Ok(Vec::new())
    }
    fn record_change(&self, _: PolicyChange) -> Result<(), GovernanceError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The site under test.
// ---------------------------------------------------------------------------------------------

struct Env {
    _db: TestDb,
    backend: PgBackend,
    catalog: Arc<Catalog>,
    pool: Pool,
    clock: Arc<ManualClock>,
}

async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    backend.prepare(&catalog).await.unwrap();
    let pool = backend.pool();
    Some(Env {
        _db: db,
        backend,
        catalog,
        pool,
        clock: Arc::new(ManualClock::new(start())),
    })
}

fn settings(admin_override: bool) -> Settings {
    let governed = |doctype: &str, mode: &str| GovernedEntry {
        doctype: doctype.to_owned(),
        scope: None,
        mode: mode.to_owned(),
    };
    Settings {
        admin_override,
        governed: vec![
            governed("invoice", "governed"),
            governed("item", "process_only"),
        ],
        entry: vec![EntryProcessEntry {
            doctype: "item".to_owned(),
            scope: None,
            process: "new_item".to_owned(),
        }],
        ..Settings::default()
    }
}

fn user(_n: u128) -> Actor {
    Actor::User {
        id: RecordId::new_v7(),
        via: None,
    }
}

async fn host(
    env: &Env,
    site: &str,
    settings: Settings,
    roles: Roles,
    secrets: DerivedSecrets,
) -> GovernanceHost {
    let policy = SitePolicy::from_settings(&settings, site).shared();
    let covered = Some(settings.covered());
    let host = Wiring {
        site: site.to_owned(),
        clock: Arc::clone(&env.clock) as Arc<dyn Clock>,
        pool: env.pool.clone(),
        settings,
        policy,
        covered,
        principals: Arc::new(roles),
        outside: Arc::new(RoleOutsideProcess),
        secrets: Arc::new(secrets),
        participants: None,
    }
    .build()
    .unwrap();
    host.ledger().prepare().await.unwrap();
    host
}

async fn simple(env: &Env, site: &str) -> GovernanceHost {
    host(
        env,
        site,
        settings(false),
        Roles::default(),
        DerivedSecrets::fixed(&[("grant-1", KEY)]),
    )
    .await
}

fn invocation(actor: &Actor, command: Option<&str>, now: OffsetDateTime) -> Invocation {
    Invocation {
        site: SiteId::new("main").unwrap(),
        actor: actor.clone(),
        command_id: command.map(|c| CommandId::new(c).unwrap()),
        language: None,
        now,
    }
}

fn document(doctype: &str) -> Document {
    Document::new(Name::new(doctype).unwrap(), RecordId::new_v7())
}

/// One write of `doc` through the guard, in its own transaction; the transaction commits when
/// the guard allows and rolls back when it refuses, as the engine does.
async fn write(
    env: &Env,
    host: &GovernanceHost,
    op: Operation,
    doc: &Document,
    actor: &Actor,
    command: Option<&str>,
) -> Result<(), spot_app_api::AppError> {
    let inv = invocation(actor, command, env.clock.now_utc());
    let intent = WriteIntent {
        doctype: doc.doctype().as_str(),
        operation: op,
        cause: WriteCause::Direct,
        node: None,
        old: None,
        new: Some(doc),
        invocation: &inv,
    };
    let mut tx = begin(env).await;
    let outcome = host.participant().before_write(&mut tx, &intent).await;
    match &outcome {
        Ok(()) => tx.commit().await.unwrap(),
        Err(_) => {
            let _ = tx.rollback().await;
        }
    }
    outcome
}

/// One create of a fresh `doctype` record.
async fn create(
    env: &Env,
    host: &GovernanceHost,
    doctype: &str,
    actor: &Actor,
    command: Option<&str>,
) -> Result<(), spot_app_api::AppError> {
    write(
        env,
        host,
        Operation::Create,
        &document(doctype),
        actor,
        command,
    )
    .await
}

/// Hands `actor` the step `node` of `run` (attempt `attempt`) for the given types.
async fn open(
    host: &GovernanceHost,
    run: &str,
    process: &str,
    node: &str,
    attempt: u32,
    types: &[&str],
    actor: &Actor,
) -> String {
    let types: Vec<String> = types.iter().map(|t| (*t).to_owned()).collect();
    host.grants()
        .open_attempt(run, process, node, attempt, &types, &actor.principal_text())
        .await
        .unwrap()
}

async fn begin(env: &Env) -> PgTransaction {
    env.backend
        .begin(
            Arc::clone(&env.catalog),
            TxOptions::new(Name::new("gov_test").unwrap(), Isolation::ReadCommitted),
        )
        .await
        .unwrap()
}

fn code_of(result: Result<(), spot_app_api::AppError>) -> String {
    result.expect_err("the write was refused").code.to_string()
}

/// Rows of the audit table (all, or one rule) or of the grant-use table (`rule` of `"use"`).
async fn count(env: &Env, site: &str, table: &str, rule: Option<&str>) -> i64 {
    let client = env.pool.get().await.unwrap();
    let sql = if table == "use" {
        "SELECT count(*) FROM spot_bpm_gov_grant_use WHERE tenant = $1 AND ($2::text IS NULL OR $2 = $2)"
    } else {
        "SELECT count(*) FROM spot_bpm_gov_audit WHERE tenant = $1 AND ($2::text IS NULL OR rule = $2)"
    };
    client.query_one(sql, &[&site, &rule]).await.unwrap().get(0)
}

async fn grant_for(
    host: &GovernanceHost,
    run: &str,
    node: &str,
    attempt: u32,
    actor: &Actor,
) -> String {
    open(host, run, "new_invoice", node, attempt, &["invoice"], actor).await
}

// ---------------------------------------------------------------------------------------------
// The governed guard.
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_direct_create_on_a_governed_doctype_is_refused_and_audited() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    let actor = user(1);
    let doc = document("invoice");
    let result = write(&env, &host, Operation::Create, &doc, &actor, None).await;
    assert_eq!(code_of(result), DENIED);
    assert_eq!(
        count(&env, "main", "audit", Some("outside_process_denied")).await,
        1
    );
    // A type the settings do not list is free: nothing is checked or recorded.
    let free = document("note");
    write(&env, &host, Operation::Create, &free, &actor, None)
        .await
        .unwrap();
    assert_eq!(count(&env, "main", "audit", None).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_create_through_a_run_succeeds_and_consumes_the_grant() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    let actor = user(1);
    let grant = grant_for(&host, "run-1", "post", 1, &actor).await;
    let doc = document("invoice");
    write(&env, &host, Operation::Create, &doc, &actor, Some(&grant))
        .await
        .unwrap();
    assert_eq!(count(&env, "main", "use", None).await, 1);
    // The grant is spent: the same grant on a second write is a replay.
    let again = document("invoice");
    let result = write(&env, &host, Operation::Create, &again, &actor, Some(&grant)).await;
    assert_eq!(code_of(result), DENIED);
    assert_eq!(
        count(&env, "main", "audit", Some("grant_replayed")).await,
        1
    );
    assert_eq!(count(&env, "main", "use", None).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_write_does_not_spend_the_grant() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    let actor = user(1);
    let grant = grant_for(&host, "run-1", "post", 1, &actor).await;
    // The write is refused after the guard allowed it (a later participant failed): the
    // transaction rolls back and the use row goes with it.
    let doc = document("invoice");
    let inv = invocation(&actor, Some(&grant), env.clock.now_utc());
    let intent = WriteIntent {
        doctype: "invoice",
        operation: Operation::Create,
        cause: WriteCause::Direct,
        node: None,
        old: None,
        new: Some(&doc),
        invocation: &inv,
    };
    let mut tx = begin(&env).await;
    host.participant()
        .before_write(&mut tx, &intent)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(count(&env, "main", "use", None).await, 0);
    write(&env, &host, Operation::Create, &doc, &actor, Some(&grant))
        .await
        .unwrap();
    assert_eq!(count(&env, "main", "use", None).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_foreign_and_stale_grants_fail() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    let alice = user(1);
    let bob = user(2);
    let grant = grant_for(&host, "run-1", "post", 1, &alice).await;
    let doc = document("invoice");

    // Another person's grant.
    let result = write(&env, &host, Operation::Create, &doc, &bob, Some(&grant)).await;
    assert_eq!(code_of(result), DENIED);
    // A changed payload or tag.
    let mut forged = grant.clone();
    forged.push('x');
    let result = write(&env, &host, Operation::Create, &doc, &alice, Some(&forged)).await;
    assert_eq!(code_of(result), DENIED);
    // A grant for a step that may write another type.
    let other = open(&host, "run-2", "p", "n", 1, &["note"], &alice).await;
    let result = write(&env, &host, Operation::Create, &doc, &alice, Some(&other)).await;
    assert_eq!(code_of(result), DENIED);
    // A newer attempt kills the older grant of the same step.
    let newer = grant_for(&host, "run-1", "post", 2, &alice).await;
    let result = write(&env, &host, Operation::Create, &doc, &alice, Some(&grant)).await;
    assert_eq!(code_of(result), DENIED);
    write(&env, &host, Operation::Create, &doc, &alice, Some(&newer))
        .await
        .unwrap();
    // A closed request kills every grant it issued.
    let closing = grant_for(&host, "run-3", "post", 1, &alice).await;
    host.grants().close_request("run-3").await.unwrap();
    let result = create(&env, &host, "invoice", &alice, Some(&closing)).await;
    assert_eq!(code_of(result), DENIED);
    // An expired grant.
    let late = grant_for(&host, "run-4", "post", 1, &alice).await;
    env.clock.set_wall(start() + time::Duration::minutes(30));
    let result = create(&env, &host, "invoice", &alice, Some(&late)).await;
    assert_eq!(code_of(result), DENIED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_id_claiming_a_process_origin_is_ignored() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    let actor = user(1);
    // The old plain form, and a grant-shaped string nobody signed.
    for claim in ["run-1:post:1", "rg1.eyJ2IjoxfQ.AAAA", "run-1"] {
        let result = create(&env, &host, "invoice", &actor, Some(claim)).await;
        assert_eq!(code_of(result), DENIED, "claim {claim}");
    }
    // Even with the step open in the ledger, the plain claim opens nothing.
    grant_for(&host, "run-1", "post", 1, &actor).await;
    let result = create(&env, &host, "invoice", &actor, Some("run-1:post:1")).await;
    assert_eq!(code_of(result), DENIED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_override_is_off_by_default_and_audited_when_on() {
    let Some(env) = env().await else { return };
    let admin = user(1);
    let plain = user(2);
    let mut roles = Roles::default();
    roles
        .0
        .insert(admin.principal_text(), vec!["System Manager".to_owned()]);
    roles
        .0
        .insert(plain.principal_text(), vec!["Clerk".to_owned()]);

    // Off: the administrator is refused like anyone.
    let off = host(
        &env,
        "main",
        settings(false),
        Roles(roles.0.clone()),
        DerivedSecrets::fixed(&[("grant-1", KEY)]),
    )
    .await;
    let result = create(&env, &off, "invoice", &admin, None).await;
    assert_eq!(code_of(result), DENIED);

    // On: the administrator succeeds, with an audit row; anybody else is still refused.
    let on = host(
        &env,
        "site-on",
        settings(true),
        Roles(roles.0),
        DerivedSecrets::fixed(&[("grant-1", KEY)]),
    )
    .await;
    create(&env, &on, "invoice", &admin, None).await.unwrap();
    assert_eq!(
        count(&env, "site-on", "audit", Some("admin_override")).await,
        1
    );
    let result = create(&env, &on, "invoice", &plain, None).await;
    assert_eq!(code_of(result), DENIED);
    // The override never reaches a process_only type.
    let result = create(&env, &on, "item", &admin, None).await;
    assert_eq!(code_of(result), START_PROCESS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_outside_process_permission_needs_a_reason() {
    let Some(env) = env().await else { return };
    let holder = user(1);
    let mut roles = Roles::default();
    roles.0.insert(
        holder.principal_text(),
        vec![OUTSIDE_PROCESS_PERMISSION.to_owned()],
    );
    let host = host(
        &env,
        "main",
        settings(false),
        roles,
        DerivedSecrets::fixed(&[("grant-1", KEY)]),
    )
    .await;

    let result = create(&env, &host, "invoice", &holder, None).await;
    assert_eq!(code_of(result), REASON);

    let mut doc = document("invoice");
    doc.set(
        Name::new(REASON_FIELD).unwrap(),
        "month-end correction approved by the controller",
    );
    write(&env, &host, Operation::Create, &doc, &holder, None)
        .await
        .unwrap();
    assert_eq!(
        count(&env, "main", "audit", Some("outside_process")).await,
        1
    );

    // A short reason is below the floor.
    let mut short = document("invoice");
    short.set(Name::new(REASON_FIELD).unwrap(), "fix");
    let result = write(&env, &host, Operation::Create, &short, &holder, None).await;
    assert_eq!(code_of(result), REASON);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_of_a_governed_record_needs_a_grant_too() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    let actor = user(1);
    let doc = document("invoice");
    let result = write(&env, &host, Operation::Update, &doc, &actor, None).await;
    assert_eq!(code_of(result), DENIED);
    let grant = grant_for(&host, "run-1", "amend", 1, &actor).await;
    write(&env, &host, Operation::Update, &doc, &actor, Some(&grant))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_only_creates_need_the_entry_process() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    let actor = user(1);
    let doc = document("item");
    let result = write(&env, &host, Operation::Create, &doc, &actor, None).await;
    assert_eq!(code_of(result), START_PROCESS);
    assert_eq!(count(&env, "main", "audit", Some("start_process")).await, 1);

    // A grant for a step of another process does not do.
    let wrong = open(&host, "run-1", "other_process", "n", 1, &["item"], &actor).await;
    let result = write(&env, &host, Operation::Create, &doc, &actor, Some(&wrong)).await;
    assert_eq!(code_of(result), START_PROCESS);

    // A grant for a step of the entry process does.
    let right = open(&host, "run-2", "new_item", "n", 1, &["item"], &actor).await;
    write(&env, &host, Operation::Create, &doc, &actor, Some(&right))
        .await
        .unwrap();

    // A change is the process's work as well.
    let result = write(&env, &host, Operation::Update, &doc, &actor, None).await;
    assert_eq!(code_of(result), DENIED);

    // The route tells a client what to start.
    match host.route("main", "item", env.clock.now_utc()).unwrap() {
        CreateRoute::StartProcess(process) => assert_eq!(process.id, "new_item"),
        CreateRoute::Direct => panic!("item is process_only"),
    }
    assert_eq!(
        host.route("main", "note", env.clock.now_utc()).unwrap(),
        CreateRoute::Direct
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_company_the_actor_cannot_use_is_refused_in_process_only() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    let mut doc = document("item");
    doc.set(Name::new("company").unwrap(), "elsewhere");
    let result = write(&env, &host, Operation::Create, &doc, &user(1), None).await;
    assert_eq!(code_of(result), "bpm_entry.scope_invalid");
    assert_eq!(count(&env, "main", "audit", Some("scope_invalid")).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_guard_fails_closed_when_the_policy_or_the_key_is_unavailable() {
    let Some(env) = env().await else { return };
    // A policy that cannot be read refuses every write the guard is asked about.
    let broken = Wiring {
        site: "main".to_owned(),
        clock: Arc::clone(&env.clock) as Arc<dyn Clock>,
        pool: env.pool.clone(),
        settings: settings(false),
        policy: Arc::new(BrokenPolicy),
        covered: None,
        principals: Arc::new(Roles::default()),
        outside: Arc::new(RoleOutsideProcess),
        secrets: Arc::new(DerivedSecrets::fixed(&[("grant-1", KEY)])),
        participants: None,
    }
    .build()
    .unwrap();
    broken.ledger().prepare().await.unwrap();
    let result = write(
        &env,
        &broken,
        Operation::Create,
        &document("invoice"),
        &user(1),
        None,
    )
    .await;
    assert_eq!(code_of(result), SETTINGS_CODE);
    assert_eq!(
        count(&env, "main", "audit", Some("policy_unavailable")).await,
        1
    );

    // A site whose key is gone issues nothing and accepts nothing.
    let keyless = host(
        &env,
        "keyless",
        settings(false),
        Roles::default(),
        DerivedSecrets::new(),
    )
    .await;
    let actor = user(1);
    let (types, who) = (["invoice".to_owned()], actor.principal_text());
    let issued = keyless
        .grants()
        .open_attempt("run-1", "p", "n", 1, &types, &who)
        .await;
    assert!(matches!(issued, Err(IssueError::Authority(_))));
    let good = simple(&env, "main").await;
    let grant = grant_for(&good, "run-9", "post", 1, &actor).await;
    let result = create(&env, &keyless, "invoice", &actor, Some(&grant)).await;
    assert_eq!(code_of(result), DENIED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rotated_key_keeps_the_grants_in_flight_valid() {
    let Some(env) = env().await else { return };
    let actor = user(1);
    let mut old = settings(false);
    old.key_ids = vec!["grant-1".to_owned()];
    let before = host(
        &env,
        "main",
        old,
        Roles::default(),
        DerivedSecrets::fixed(&[("grant-1", KEY)]),
    )
    .await;
    let grant = grant_for(&before, "run-1", "post", 1, &actor).await;

    // After rotation the new id signs, and the old id still verifies.
    let mut rotated = settings(false);
    rotated.key_ids = vec!["grant-2".to_owned(), "grant-1".to_owned()];
    let after = host(
        &env,
        "main",
        rotated,
        Roles::default(),
        DerivedSecrets::fixed(&[
            ("grant-1", KEY),
            ("grant-2", b"fedcba9876543210fedcba9876543210"),
        ]),
    )
    .await;
    after.refresh(None).await;
    create(&env, &after, "invoice", &actor, Some(&grant))
        .await
        .unwrap();
    // A grant signed under an id that is no longer listed does not verify.
    let mut retired = settings(false);
    retired.key_ids = vec!["grant-2".to_owned()];
    let gone = host(
        &env,
        "main",
        retired,
        Roles::default(),
        DerivedSecrets::fixed(&[("grant-2", b"fedcba9876543210fedcba9876543210")]),
    )
    .await;
    let second = grant_for(&before, "run-2", "post", 1, &actor).await;
    gone.refresh(None).await;
    let result = create(&env, &gone, "invoice", &actor, Some(&second)).await;
    assert_eq!(code_of(result), DENIED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sites_share_nothing() {
    let Some(env) = env().await else { return };
    let a = simple(&env, "site-a").await;
    let b = simple(&env, "site-b").await;
    let actor = user(1);
    let grant = grant_for(&a, "run-1", "post", 1, &actor).await;
    // The grant of site A authenticates on site B's key (same bytes here) but names a request
    // site B's ledger does not hold: nothing opens.
    let result = create(&env, &b, "invoice", &actor, Some(&grant)).await;
    assert_eq!(code_of(result), DENIED);
    assert_eq!(count(&env, "site-b", "use", None).await, 0);
    create(&env, &a, "invoice", &actor, Some(&grant))
        .await
        .unwrap();
    assert_eq!(count(&env, "site-a", "use", None).await, 1);
    assert_eq!(count(&env, "site-b", "use", None).await, 0);
    // A step opened on site B is invisible to A's cache until A's ledger says so (it never does).
    open(&b, "run-b", "p", "n", 1, &["invoice"], &actor).await;
    a.refresh(None).await;
    assert_eq!(a.grants().refresh().await.unwrap(), 1);
}

// ---------------------------------------------------------------------------------------------
// Drafts.
// ---------------------------------------------------------------------------------------------

/// Saves the draft `id` of `owner` for the step `enter` of `run`.
async fn put(
    drafts: &DraftStore,
    env: &Env,
    id: &str,
    run: &str,
    owner: &Actor,
    note: &str,
) -> Result<(), DraftError> {
    drafts
        .save(
            id,
            run,
            "enter",
            "invoice",
            &owner.principal_text(),
            &values(note),
            env.clock.now_utc(),
        )
        .await
}

fn values(note: &str) -> Map<String, serde_json::Value> {
    let mut map = Map::new();
    map.insert("note".to_owned(), json!(note));
    map
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_is_private_then_shared_with_the_runs_participants_then_reaped() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    let alice = user(1);
    let bob = user(2);
    let carol = user(3);
    let drafts = host.drafts();
    put(drafts, &env, "draft-1", "run-1", &alice, "hello")
        .await
        .unwrap();

    // Only the requester sees a draft.
    let mine = drafts
        .get("draft-1", &alice.principal_text())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mine.state, DraftState::Draft);
    assert_eq!(mine.values["note"], json!("hello"));
    assert!(
        drafts
            .get("draft-1", &bob.principal_text())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        drafts
            .get("missing", &bob.principal_text())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        drafts.list(&alice.principal_text(), 10).await.unwrap(),
        vec!["draft-1".to_owned()]
    );
    assert!(
        drafts
            .list(&bob.principal_text(), 10)
            .await
            .unwrap()
            .is_empty()
    );

    // Somebody else cannot save over it or submit it.
    let over = put(drafts, &env, "draft-1", "run-1", &bob, "mine now").await;
    assert_eq!(over, Err(DraftError::NotOpen));
    assert_eq!(
        drafts
            .submit("draft-1", &bob.principal_text(), env.clock.now_utc())
            .await,
        Err(DraftError::NotYours)
    );

    // Submitted: Bob took part in the Run (a step was handed to him), Carol did not.
    drafts
        .submit("draft-1", &alice.principal_text(), env.clock.now_utc())
        .await
        .unwrap();
    open(&host, "run-1", "p", "approve", 1, &[], &bob).await;
    assert!(
        drafts
            .get("draft-1", &bob.principal_text())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        drafts
            .get("draft-1", &carol.principal_text())
            .await
            .unwrap()
            .is_none()
    );
    // A submitted draft can no longer be saved over by its requester.
    let again = put(drafts, &env, "draft-1", "run-1", &alice, "late").await;
    assert_eq!(again, Err(DraftError::NotOpen));

    // Thirty-one days later the reaper removes it; a fresh draft stays.
    put(drafts, &env, "draft-2", "run-2", &alice, "old")
        .await
        .unwrap();
    env.clock.set_wall(start() + time::Duration::days(31));
    put(drafts, &env, "draft-3", "run-3", &alice, "new")
        .await
        .unwrap();
    assert_eq!(host.reap().await, 2);
    assert!(
        drafts
            .get("draft-1", &alice.principal_text())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        drafts
            .get("draft-2", &alice.principal_text())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        drafts
            .get("draft-3", &alice.principal_text())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_of_one_site_is_invisible_to_another() {
    let Some(env) = env().await else { return };
    let a = simple(&env, "site-a").await;
    let b = simple(&env, "site-b").await;
    let alice = user(1);
    put(a.drafts(), &env, "draft-1", "run-1", &alice, "x")
        .await
        .unwrap();
    assert!(
        b.drafts()
            .get("draft-1", &alice.principal_text())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        a.drafts()
            .get("draft-1", &alice.principal_text())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_form_steps_draft_writer_commits_with_the_transaction() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    let alice = user(1);
    let owner = alice.principal_text();
    let save = |id: RecordId, mode: DraftMode, note: &str| DraftSave {
        mode,
        doctype: "invoice".to_owned(),
        document: id,
        expected: None,
        values: values(note),
        actor: owner.clone(),
        context: RequestContext {
            request: "run-1".to_owned(),
            node: "enter".to_owned(),
            attempt: 1,
            token: "unused".to_owned(),
        },
        now: env.clock.now_utc(),
    };
    let writer = host.draft_writer();
    let id = RecordId::new_v7();

    // Rolled back with the transaction: nothing is stored.
    let mut tx = begin(&env).await;
    Arc::clone(&writer)
        .write(&mut tx, Arc::new(save(id, DraftMode::Create, "first")))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(
        host.drafts()
            .get(&id.to_string(), &owner)
            .await
            .unwrap()
            .is_none()
    );

    // Committed with it: stored, and an update replaces the values.
    let mut tx = begin(&env).await;
    Arc::clone(&writer)
        .write(&mut tx, Arc::new(save(id, DraftMode::Create, "first")))
        .await
        .unwrap();
    Arc::clone(&writer)
        .write(&mut tx, Arc::new(save(id, DraftMode::Update, "second")))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let stored = host
        .drafts()
        .get(&id.to_string(), &owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.values["note"], json!("second"));
    assert_eq!(stored.doctype, "invoice");

    // The writer's validation is the draft's own: an oversized value is refused before any write.
    let mut big = save(RecordId::new_v7(), DraftMode::Create, "x");
    big.values.insert(
        "note".to_owned(),
        json!("x".repeat(spot_cli::services::process_governance::MAX_DRAFT_BYTES)),
    );
    assert!(writer.validate(&big).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tables_prepare_twice_and_a_bad_grant_is_refused_before_any_write() {
    let Some(env) = env().await else { return };
    let host = simple(&env, "main").await;
    host.ledger().prepare().await.unwrap();
    let grants = host.grants();
    for (run, actor) in [(" ", "user:1"), ("run", "alice")] {
        let refused = grants.open_attempt(run, "p", "n", 1, &[], actor).await;
        assert!(matches!(refused, Err(IssueError::Authority(_))));
    }
    assert_eq!(grants.refresh().await.unwrap(), 0);
}
