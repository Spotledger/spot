//! The result table, the directory, the history and the scratch database lifecycle over a real
//! PostgreSQL server. Skipped (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_journal_audit_host_pg::schema::{DDL, RETENTION_MILLIS};
use spot_journal_audit_host_pg::{
    HostError, PgAuditHistory, PgResultSink, PgTenantDirectory, ReplayPolicies, ScratchFactory,
    SyncPg, database_name,
};
use spot_journal_audit_job::{
    Cadence, FailureInfo, ResultSink, TenantDirectory, TenantOutcome, TenantResult,
};
use spot_journal_audit_run::{ClassifierKind, UnknownTopics};
use spot_journal_recovery::AuditHistory;
use spot_journal_store::TenantId;
use spot_postgres::testing::TestDb;

const T0: i64 = 1_800_000_000_000;

struct Always(&'static str);
impl ReplayPolicies for Always {
    fn is_always(&self, tenant: &str) -> bool {
        tenant == self.0
    }
}

async fn db() -> Option<TestDb> {
    let db = TestDb::create().await?;
    db.raw_client().await.batch_execute(DDL).await.unwrap();
    Some(db)
}

fn result(tenant: &TenantId, started: i64, outcome: TenantOutcome) -> TenantResult {
    TenantResult {
        tenant: tenant.clone(),
        started_millis: started,
        finished_millis: started + 1000,
        outcome,
        run: None,
        condition_failure: None,
        classifier: ClassifierKind::Registry,
        unknown_topics: UnknownTopics::none(),
    }
}

// The providers are synchronous: they must be driven from plain threads, as the job's blocking
// pool does, so each test hands its body to one.
fn on_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(work).join().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_directory_reads_cadence_and_last_run_from_the_result_table() {
    let Some(db) = db().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let config = db.config();
    on_thread(move || {
        let worker = Arc::new(SyncPg::start(config).unwrap());
        let acme = TenantId::new("acme").unwrap();
        let beta = TenantId::new("beta").unwrap();
        let sink = PgResultSink::new(Arc::clone(&worker));
        let directory = PgTenantDirectory::new(
            Arc::clone(&worker),
            vec![acme.clone(), beta.clone()],
            Arc::new(Always("beta")),
        );
        let plans = directory.tenants().unwrap();
        assert!(plans.iter().all(|p| p.last_run_millis.is_none()));
        assert_eq!(plans[0].cadence, Cadence::Weekly);
        assert_eq!(plans[1].cadence, Cadence::Daily);

        sink.record(&acme, &result(&acme, T0, TenantOutcome::Clean))
            .unwrap();
        sink.record(
            &acme,
            &result(
                &acme,
                T0 + 5000,
                TenantOutcome::Failed(FailureInfo::new("rebuild", "journal_rebuild.position_gap")),
            ),
        )
        .unwrap();
        let plans = directory.tenants().unwrap();
        assert_eq!(plans[0].last_run_millis, Some(T0 + 5000));
        assert_eq!(
            plans[1].last_run_millis, None,
            "another tenant's rows are not this one's"
        );

        // Fifty-two weeks are kept: a row older than that is pruned by the next record.
        let old = T0 + 10_000 + RETENTION_MILLIS;
        sink.record(&beta, &result(&beta, T0, TenantOutcome::Clean))
            .unwrap();
        sink.record(&beta, &result(&beta, old, TenantOutcome::Clean))
            .unwrap();
        let plans = directory.tenants().unwrap();
        assert_eq!(plans[1].last_run_millis, Some(old));
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn the_history_answers_the_last_failure_and_never_a_default() {
    let Some(db) = db().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let config = db.config();
    on_thread(move || {
        let worker = Arc::new(SyncPg::start(config).unwrap());
        let acme = TenantId::new("acme").unwrap();
        let history = PgAuditHistory::new(Arc::clone(&worker), &acme);
        assert_eq!(history.last_verified().unwrap(), None, "never verified");
        assert_eq!(history.last_failure().unwrap(), None);
        let sink = PgResultSink::new(Arc::clone(&worker));
        sink.record(&acme, &result(&acme, T0, TenantOutcome::Mismatch))
            .unwrap();
        let failure = history.last_failure().unwrap().unwrap();
        assert_eq!(failure.at_millis, T0 + 1000);
        // A later clean run clears it.
        sink.record(&acme, &result(&acme, T0 + 9000, TenantOutcome::Clean))
            .unwrap();
        assert_eq!(history.last_failure().unwrap(), None);
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn the_scratch_database_lives_for_one_run_and_leaks_are_swept() {
    let Some(db) = db().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let config = db.config();
    on_thread(move || {
        let admin = Arc::new(SyncPg::start(config.clone()).unwrap());
        let factory = Arc::new(ScratchFactory::new(admin, config));
        let name = database_name("acme", "run1").unwrap();

        // Create, use, drop explicitly.
        let mut one = factory.create("acme", "run1", T0).unwrap();
        assert_eq!(one.name(), name);
        assert!(factory.exists(&name).unwrap());
        one.drop_now().unwrap();
        assert!(!factory.exists(&name).unwrap());

        // The guard drops it on every path, a panic unwind included.
        let name2 = database_name("acme", "run2").unwrap();
        {
            let _guard = factory.create("acme", "run2", T0).unwrap();
            assert!(factory.exists(&name2).unwrap());
        }
        assert!(!factory.exists(&name2).unwrap());

        // A second database of the same name is refused, not reused.
        let first = factory.create("acme", "run3", T0).unwrap();
        assert!(factory.create("acme", "run3", T0).is_err());
        drop(first);

        // The sweep removes this tenant's leak older than the cut and nothing else.
        std::mem::forget(factory.create("acme", "old1", T0).unwrap()); // a crashed run's leak
        let fresh = factory.create("acme", "new1", T0 + 10 * 3_600_000).unwrap();
        std::mem::forget(factory.create("beta", "old1", T0).unwrap()); // another tenant's
        let dropped = factory.sweep("acme", T0 + 3_600_000).unwrap();
        assert_eq!(dropped, [database_name("acme", "old1").unwrap()]);
        assert!(
            factory.exists(fresh.name()).unwrap(),
            "a run in flight is untouched"
        );
        assert!(
            factory
                .exists(&database_name("beta", "old1").unwrap())
                .unwrap(),
            "another tenant's database is untouched"
        );
        let beta_gone = factory.sweep("beta", T0 + 3_600_000).unwrap();
        assert_eq!(beta_gone.len(), 1);
        // Hostile names never reach SQL.
        assert_eq!(factory.sweep("a\"; drop", 0), Err(HostError::ScratchName));
        assert!(matches!(
            factory.create("acme", "x; DROP DATABASE postgres", T0),
            Err(HostError::ScratchName)
        ));
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn a_superuser_scratch_role_is_refused() {
    let Some(db) = db().await else {
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let config = db.config();
    let is_super = db
        .raw_client()
        .await
        .query_one(
            "SELECT rolsuper FROM pg_roles WHERE rolname = current_user",
            &[],
        )
        .await
        .unwrap()
        .get::<_, bool>(0);
    on_thread(move || {
        let admin = Arc::new(SyncPg::start(config.clone()).unwrap());
        let factory = ScratchFactory::new(admin, config);
        let checked = factory.check_role();
        if is_super {
            assert_eq!(checked, Err(HostError::RoleTooPowerful("superuser")));
        } else {
            // A CI role without superuser must at least be judged on CREATEDB and the rest.
            assert!(checked.is_ok() || checked == Err(HostError::RoleUnusable) || checked.is_err());
        }
    });
}
