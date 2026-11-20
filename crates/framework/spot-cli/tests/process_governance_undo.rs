//! A request stays open until its undo has finished (WP-BPM-15.T6): closing the request while an
//! undo is pending is deferred and durable; the undo can still be handed a grant while it runs;
//! `failed` is audited with its code and closes the request; a crash between accept and finish
//! leaves the request open and the retry is idempotent by key.
//!
//! Needs a real PostgreSQL server (`SPOT_TEST_PG_URL`); skips with `SKIPPED:` without it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use deadpool_postgres::Pool;
use spot_app_api::BoxFuture;
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use spot_cli::services::process_governance::{
    DerivedSecrets, GovernanceHost, GovernedEntry, PrincipalLookup, ReversalOutcome, ReversalState,
    Settings, SitePolicy, Wiring,
};
use spot_core::{Clock, ManualClock};
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const SITE: &str = "acme";
const CODE: &str = "bpm_effects.cancellation_failed";

struct NoRoles;

impl PrincipalLookup for NoRoles {
    fn principal<'a>(
        &'a self,
        actor: &'a spot_app_api::context::Actor,
    ) -> BoxFuture<'a, Principal> {
        Box::pin(async move { Principal::with_roles(&actor.principal_text(), &[]) })
    }
}

struct Env {
    _db: TestDb,
    pool: Pool,
    clock: Arc<ManualClock>,
}

async fn env() -> Option<Env> {
    let db = match TestDb::create().await {
        Some(db) => db,
        None => {
            println!("SKIPPED: no PostgreSQL available (set SPOT_TEST_PG_URL)");
            return None;
        }
    };
    let mut cfg = db.config();
    cfg.application_name = "gov-undo".to_owned();
    cfg.pool_max_size = 6;
    let pool = PgBackend::connect_unchecked(cfg).unwrap().pool();
    Some(Env {
        _db: db,
        pool,
        clock: Arc::new(ManualClock::new(
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        )),
    })
}

/// A host over the shared database; building a second one is a restart of the node.
async fn host(env: &Env) -> Arc<GovernanceHost> {
    let settings = Settings {
        governed: vec![GovernedEntry {
            doctype: "invoice".to_owned(),
            scope: None,
            mode: "governed".to_owned(),
        }],
        ..Settings::default()
    };
    let policy = SitePolicy::from_settings(&settings, SITE).shared();
    let covered = Some(settings.covered());
    let host = Wiring {
        site: SITE.to_owned(),
        clock: Arc::clone(&env.clock) as Arc<dyn Clock>,
        pool: env.pool.clone(),
        settings,
        policy,
        covered,
        principals: Arc::new(NoRoles),
        outside: Arc::new(spot_bpm_governance::RoleOutsideProcess),
        secrets: Arc::new(DerivedSecrets::fixed(&[("grant-1", KEY)])),
        participants: None,
    }
    .build()
    .unwrap();
    host.ledger().prepare().await.unwrap();
    let host = Arc::new(host);
    // A restarted node reloads its cache of open steps from the ledger.
    host.grants().refresh().await.unwrap();
    host
}

fn who() -> String {
    format!("user:{}", RecordId::new_v7())
}

async fn open_grant(host: &GovernanceHost, request: &str, actor: &str) -> String {
    host.grants()
        .open_attempt(
            request,
            "process",
            "undo",
            1,
            &["invoice".to_owned()],
            actor,
        )
        .await
        .unwrap()
}

fn valid(host: &GovernanceHost, grant: &str, actor: &str) -> bool {
    host.grants()
        .authority()
        .authenticate(grant, actor)
        .is_some()
}

async fn count(env: &Env, sql: &str, request: &str) -> i64 {
    let client = env.pool.get().await.unwrap();
    client
        .query_one(sql, &[&SITE, &request])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn a_close_waits_for_the_undo_and_happens_after_it_not_before() {
    let Some(env) = env().await else { return };
    let host = host(&env).await;
    let actor = who();
    let grant = open_grant(&host, "r1", &actor).await;
    assert_eq!(
        host.grants().begin_reversal("r1", "undo-1").await.unwrap(),
        ReversalState::Pending
    );
    // The Run ends while the undo is running: the request must stay open.
    host.grants().close_request("r1").await.unwrap();
    assert!(
        valid(&host, &grant, &actor),
        "still open while the undo runs"
    );
    host.grants()
        .finish_reversal("r1", "undo-1", ReversalOutcome::Done)
        .await
        .unwrap();
    assert!(
        !valid(&host, &grant, &actor),
        "closed after the undo, not before"
    );
}

#[tokio::test]
async fn a_request_with_no_undo_closes_at_once() {
    let Some(env) = env().await else { return };
    let host = host(&env).await;
    let actor = who();
    let grant = open_grant(&host, "r2", &actor).await;
    host.grants().close_request("r2").await.unwrap();
    assert!(!valid(&host, &grant, &actor));
}

#[tokio::test]
async fn a_failed_undo_is_audited_with_its_code_and_closes_the_request() {
    let Some(env) = env().await else { return };
    let host = host(&env).await;
    let actor = who();
    let grant = open_grant(&host, "r3", &actor).await;
    host.grants().begin_reversal("r3", "undo-1").await.unwrap();
    host.grants()
        .finish_reversal(
            "r3",
            "undo-1",
            ReversalOutcome::Failed {
                code: CODE.to_owned(),
            },
        )
        .await
        .unwrap();
    assert!(!valid(&host, &grant, &actor));
    let audited = {
        let client = env.pool.get().await.unwrap();
        client
            .query_one(
                "SELECT count(*) FROM spot_bpm_gov_audit WHERE tenant = $1 AND kind = 'undo_failed' AND rule = $2 AND document_id = $3",
                &[&SITE, &CODE, &"r3"],
            )
            .await
            .unwrap()
            .get::<_, i64>(0)
    };
    assert_eq!(audited, 1, "nothing is closed silently");
    // Repeating the finish changes and audits nothing more.
    host.grants()
        .finish_reversal(
            "r3",
            "undo-1",
            ReversalOutcome::Failed {
                code: CODE.to_owned(),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_crash_between_accept_and_finish_leaves_it_open_and_resumes_one_undo() {
    let Some(env) = env().await else { return };
    let first = host(&env).await;
    let actor = who();
    let grant = open_grant(&first, "r4", &actor).await;
    first.grants().begin_reversal("r4", "undo-1").await.unwrap();
    first.grants().close_request("r4").await.unwrap();
    drop(first); // the node dies here

    let second = host(&env).await;
    assert!(
        valid(&second, &grant, &actor),
        "the request is still open after the restart"
    );
    // The retry from the stored intent: the same key, still pending, one row.
    assert_eq!(
        second
            .grants()
            .begin_reversal("r4", "undo-1")
            .await
            .unwrap(),
        ReversalState::Pending
    );
    assert_eq!(
        count(
            &env,
            "SELECT count(*) FROM spot_bpm_gov_reversal WHERE tenant = $1 AND request = $2",
            "r4"
        )
        .await,
        1
    );
    second
        .grants()
        .finish_reversal("r4", "undo-1", ReversalOutcome::Done)
        .await
        .unwrap();
    assert!(!valid(&second, &grant, &actor));
    // A late retry of a finished undo reports its state and starts nothing.
    assert_eq!(
        second
            .grants()
            .begin_reversal("r4", "undo-1")
            .await
            .unwrap(),
        ReversalState::Done
    );
}

#[tokio::test]
async fn a_close_whose_finish_was_recorded_but_not_carried_out_is_finished_on_refresh() {
    let Some(env) = env().await else { return };
    let host = host(&env).await;
    let actor = who();
    let grant = open_grant(&host, "r5", &actor).await;
    host.grants().begin_reversal("r5", "undo-1").await.unwrap();
    host.grants().close_request("r5").await.unwrap();
    // The finish reaches the ledger and the node stops before it closes.
    host.ledger()
        .finish_reversal("r5", "undo-1", "done", "", 1)
        .await
        .unwrap();
    assert!(valid(&host, &grant, &actor));
    assert_eq!(host.grants().close_ready().await.unwrap(), 1);
    assert!(!valid(&host, &grant, &actor));
}

#[tokio::test]
async fn two_undos_keep_the_request_open_until_both_finish() {
    let Some(env) = env().await else { return };
    let host = host(&env).await;
    let actor = who();
    let grant = open_grant(&host, "r6", &actor).await;
    host.grants().begin_reversal("r6", "a").await.unwrap();
    host.grants().begin_reversal("r6", "b").await.unwrap();
    host.grants().close_request("r6").await.unwrap();
    host.grants()
        .finish_reversal("r6", "a", ReversalOutcome::Done)
        .await
        .unwrap();
    assert!(valid(&host, &grant, &actor));
    host.grants()
        .finish_reversal("r6", "b", ReversalOutcome::Done)
        .await
        .unwrap();
    assert!(!valid(&host, &grant, &actor));
}
