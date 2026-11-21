//! WP-AI-09.T3: agent memory hosted on a site (real PostgreSQL with pgvector through
//! `SPOT_TEST_PG_URL`; every test skips cleanly without it, and a server without pgvector is a skip
//! unless `SPOT_REQUIRE_PG=1`). The acceptance of WP-AI-09: no business data in the memory
//! namespace, and a learned remedy is blocked until CI passes.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_agent_memory::testing::{
    FakeEmbedder, RETRY_FAILED, STUCK_RUNNING, embedder, episode_saying, meta, sanitiser,
};
use spot_agent_memory::{
    CandidateId, CiOutcome, CiPromotion, CiRunId, Confirmation, DAY_MS, Embedder, Entry, EpisodeId,
    LearningPolicy, MemoryKind, Outcome, Procedure, ProcedureId, ProcedureInput, ProcedureStep,
    PromotionPort, RemedyId, RetentionPolicy, SourceKind, Symptom, Tier, propose_remedy,
};
use spot_agent_memory_pg::{PgMemoryProvisioner, pool_for};
use spot_app_api::context::Actor;
use spot_cli::services::agent_memory::facade::{AgentMemory, Limits, MemoryFault, Parts};
use spot_cli::services::agent_memory::filter::{Fut, RecordValues, WriteFilter};
use spot_cli::services::agent_memory::host::{HostFailure, provision_and_open};
use spot_core::{ManualClock, SiteId};
use spot_mcp::principal::McpPrincipal;
use spot_postgres::PgConfig;
use spot_postgres::testing::TestDb;
use spot_storage_contract::RecordId;
use time::macros::datetime;

const SECRET: &str = "memory-role-secret-0001";

fn runtime() -> Arc<tokio::runtime::Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap(),
    )
}

/// Record values the write filter compares against.
struct Values(Vec<String>);

impl RecordValues for Values {
    fn values<'a>(&'a self, _doctype: &'a str, _id: &'a str) -> Fut<'a, Vec<String>> {
        Box::pin(async move { self.0.clone() })
    }
}

/// A provisioned tenant memory on a scratch server, deprovisioned when the test ends.
struct Fixture {
    db: TestDb,
    rt: Arc<tokio::runtime::Runtime>,
    admin: PgConfig,
    tenants: Vec<spot_agent_memory::TenantId>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let (admin, rt, tenants) = (
            self.admin.clone(),
            Arc::clone(&self.rt),
            self.tenants.clone(),
        );
        let _ = std::thread::spawn(move || {
            let provisioner = PgMemoryProvisioner::new(admin, "und", rt);
            for tenant in tenants {
                let _ = provisioner.deprovision(&tenant);
            }
        })
        .join();
    }
}

fn require_pg() -> bool {
    std::env::var("SPOT_REQUIRE_PG").as_deref() == Ok("1")
}

impl Fixture {
    fn start() -> Option<Fixture> {
        let rt = runtime();
        // Never dropped for good: dropping a runtime inside an async test panics.
        std::mem::forget(Arc::clone(&rt));
        let db = tokio::task::block_in_place(|| rt.block_on(TestDb::create()))?;
        let admin = db.config();
        Some(Fixture {
            db,
            rt,
            admin,
            tenants: Vec::new(),
        })
    }

    fn tenant(&self, suffix: &str) -> spot_agent_memory::TenantId {
        let name = self.db.name().trim_start_matches("spot_test_");
        let tail = &name[name.len().saturating_sub(12)..];
        spot_agent_memory::TenantId::parse(&format!("t-{tail}-{suffix}")).unwrap()
    }

    /// Provisions `suffix`'s memory and builds the facade over it; `None` (after saying why) when
    /// the server has no pgvector.
    fn memory(
        &mut self,
        suffix: &str,
        values: Vec<String>,
        retention: RetentionPolicy,
        holds: Option<std::path::PathBuf>,
    ) -> Option<AgentMemory> {
        let tenant = self.tenant(suffix);
        let opened = tokio::task::block_in_place(|| {
            provision_and_open(
                &self.admin,
                "und",
                &tenant,
                SECRET,
                1_000,
                Arc::clone(&self.rt),
            )
        });
        self.tenants.push(tenant.clone());
        let opened = match opened {
            Ok(opened) => opened,
            Err(HostFailure::ExtensionMissing) => {
                assert!(
                    !require_pg(),
                    "SPOT_REQUIRE_PG=1 but the server has no pgvector"
                );
                println!("SKIPPED: the test server has no pgvector extension");
                return None;
            }
            Err(failure) => panic!("provisioning failed: {}", failure.degraded_text()),
        };
        let clock = Arc::new(ManualClock::new(datetime!(2026-11-20 10:00:00 UTC)));
        Some(AgentMemory::new(Parts {
            tenant,
            store: opened.store,
            embedder: Arc::new(FakeEmbedder::new()) as Arc<dyn Embedder>,
            filter: WriteFilter::new(
                "acme",
                sanitiser(),
                Some(Arc::new(Values(values))),
                None,
                clock.clone(),
            ),
            clock,
            retention,
            holds_file: holds,
            limits: Limits { max_k: 10 },
        }))
    }
}

fn agent(tier: Tier) -> McpPrincipal {
    McpPrincipal::new_agent(
        "agent-user",
        Actor::User {
            id: RecordId::new_v7(),
            via: None,
        },
        Vec::new(),
        "agent-1",
        tier,
    )
}

fn person() -> McpPrincipal {
    McpPrincipal::new(
        "a-person",
        Actor::User {
            id: RecordId::new_v7(),
            via: None,
        },
        Vec::new(),
    )
}

/// A procedure learned at `tier` with three resolved confirmations.
fn procedure(tenant: &str, tier: Tier) -> Procedure {
    let confirmations = (1..=3)
        .map(|n| {
            Confirmation::new(
                EpisodeId::parse(&format!("episode.e{n}")).unwrap(),
                Outcome::Resolved,
                1_000 + n,
            )
            .unwrap()
        })
        .collect();
    Procedure::record(
        &sanitiser(),
        &embedder(),
        ProcedureInput {
            id: ProcedureId::parse("procedure.stuck").unwrap(),
            title: "retry the stuck job",
            symptoms: vec![Symptom::parse(STUCK_RUNNING).unwrap()],
            steps: vec![ProcedureStep::new(RemedyId::new(RETRY_FAILED))],
            confirmations,
            meta: meta(tenant, tier, 1_000, SourceKind::RemedyRun),
        },
    )
    .unwrap()
}

// ---- Provisioning and the database boundary ----

#[test]
fn provisioning_is_idempotent_and_the_memory_role_cannot_read_a_business_table() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };
    let Some(memory) = fixture.memory("iso", Vec::new(), RetentionPolicy::default(), None) else {
        return;
    };
    // A second provisioning (a restart, or a resume after a crash) converges on the same placement.
    let tenant = memory.tenant().clone();
    let again = provision_and_open(
        &fixture.admin,
        "und",
        &tenant,
        SECRET,
        9_000,
        Arc::clone(&fixture.rt),
    )
    .unwrap();
    assert!(again.placement.database.starts_with("spot_memory_db_"));
    assert!(again.placement.role.starts_with("spot_memory_role_"));

    // A business database with one table nobody but its owner may read.
    let suffix = tenant
        .as_str()
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>();
    let business = format!("spot_biz_{}", &suffix[suffix.len().saturating_sub(10)..]);
    let rt = Arc::clone(&fixture.rt);
    rt.block_on(spot_postgres::create_database(
        &fixture.admin,
        &business,
        "und",
    ))
    .unwrap();
    let setup = rt.block_on(
        fixture
            .db
            .raw_client_on(&fixture.admin.clone().with_dbname(&business)),
    );
    rt.block_on(setup.batch_execute(
        "CREATE TABLE business_job (id text PRIMARY KEY, amount numeric(20, 0) NOT NULL);\n\
         INSERT INTO business_job VALUES ('job-1', 12500);\n\
         REVOKE ALL ON business_job FROM PUBLIC",
    ))
    .unwrap();

    // The memory role may connect to the business database (the control plane decides whether
    // PUBLIC CONNECT is revoked there) but is granted nothing on its table: PostgreSQL refuses.
    let config = again
        .placement
        .config(&fixture.admin, SECRET)
        .with_dbname(&business);
    let outcome: Result<Result<(), Option<String>>, ()> = rt.block_on(async {
        let pool = pool_for(&config).unwrap();
        let Ok(client) = pool.get().await else {
            return Err(());
        };
        Ok(
            match client.query("SELECT amount FROM business_job", &[]).await {
                Ok(_) => Ok(()),
                Err(error) => Err(error.code().map(|state| state.code().to_owned())),
            },
        )
    });
    match outcome {
        // CONNECT revoked on the business database is a refusal too.
        Err(()) => {}
        Ok(Ok(())) => panic!("the memory role read a business table"),
        Ok(Err(code)) => assert_eq!(code.as_deref(), Some("42501"), "permission denied expected"),
    }
    let cleanup = rt.block_on(fixture.db.admin_client());
    let _ = rt.block_on(cleanup.batch_execute(&format!(
        "DROP DATABASE IF EXISTS \"{business}\" WITH (FORCE)"
    )));
}

#[test]
fn the_missing_pgvector_report_names_the_fix() {
    let text = HostFailure::ExtensionMissing.degraded_text();
    assert!(
        text.contains("pgvector") && text.contains("Install"),
        "{text}"
    );
    assert!(
        HostFailure::Password
            .degraded_text()
            .contains("password_file")
    );
}

// ---- No business data in the memory namespace ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_containing_a_document_field_value_is_refused_counted_and_not_stored() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };
    let Some(memory) = fixture.memory(
        "wr",
        vec!["Wavelength Textiles Pvt".to_owned()],
        RetentionPolicy::default(),
        None,
    ) else {
        return;
    };
    let tenant = memory.tenant().as_str().to_owned();
    let caller = agent(Tier::T2);
    let clean = Entry::Episode(episode_saying(
        &tenant,
        1,
        "the running job was unstuck",
        1_000,
    ));
    memory.remember(&caller, clean).await.unwrap();

    let leaking = Entry::Episode(episode_saying(
        &tenant,
        2,
        "customer wavelength textiles pvt lost its job",
        1_000,
    ));
    let refused = memory.remember(&caller, leaking).await.unwrap_err();
    assert_eq!(refused, MemoryFault::BusinessData("document_field_value"));
    assert!(!format!("{refused:?}").to_lowercase().contains("wavelength"));
    let counters = memory.filter().counters();
    assert_eq!((counters.accepted(), counters.refused()), (1, 1));
    let store = Arc::clone(memory.store());
    let kept = tokio::task::spawn_blocking(move || store.count(MemoryKind::Episode).unwrap())
        .await
        .unwrap();
    assert_eq!(kept, 1, "the refused entry never reached the database");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_path_is_tier_checked_for_writes_and_recall() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };
    let Some(memory) = fixture.memory("tier", Vec::new(), RetentionPolicy::default(), None) else {
        return;
    };
    let tenant = memory.tenant().as_str().to_owned();
    // Memory serves agents: a person, and the platform, are refused.
    let query = "retry the stuck job";
    assert_eq!(
        memory.recall(&person(), query, 3, &[]).await.unwrap_err(),
        MemoryFault::AgentsOnly
    );
    // An agent never writes above its own tier.
    let high = Entry::Procedure(procedure(&tenant, Tier::T2));
    assert_eq!(
        memory
            .remember(&agent(Tier::T0), high.clone())
            .await
            .unwrap_err(),
        MemoryFault::TierRefused
    );
    memory.remember(&agent(Tier::T2), high).await.unwrap();
    // A write for another tenant is refused before the filter or the database.
    let foreign = Entry::Procedure(procedure("another-tenant", Tier::T0));
    assert_eq!(
        memory
            .remember(&agent(Tier::T2), foreign)
            .await
            .unwrap_err(),
        MemoryFault::OtherTenant
    );

    let low = memory
        .recall(&agent(Tier::T0), query, 5, &[MemoryKind::Procedure])
        .await
        .unwrap();
    assert!(
        low.is_empty(),
        "a procedure above the caller's tier is not returned: {low:?}"
    );
    let high = memory
        .recall(&agent(Tier::T2), query, 5, &[MemoryKind::Procedure])
        .await
        .unwrap();
    assert_eq!(high.len(), 1, "the agent at the learning tier recalls it");
    assert_eq!(high[0].provenance.tier, Tier::T2);
    assert_eq!(
        memory
            .recall(&agent(Tier::T2), query, 0, &[])
            .await
            .unwrap_err(),
        MemoryFault::Invalid("k is out of range")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_tenants_memory_is_a_database_of_its_own() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };
    let Some(a) = fixture.memory("a", Vec::new(), RetentionPolicy::default(), None) else {
        return;
    };
    let Some(b) = fixture.memory("b", Vec::new(), RetentionPolicy::default(), None) else {
        return;
    };
    let tenant_a = a.tenant().as_str().to_owned();
    a.remember(
        &agent(Tier::T0),
        Entry::Episode(episode_saying(
            &tenant_a,
            1,
            "the running job was unstuck",
            1_000,
        )),
    )
    .await
    .unwrap();
    let found_a = a
        .recall(&agent(Tier::T0), "the running job was unstuck", 5, &[])
        .await
        .unwrap();
    let found_b = b
        .recall(&agent(Tier::T0), "the running job was unstuck", 5, &[])
        .await
        .unwrap();
    assert_eq!(found_a.len(), 1);
    assert!(found_b.is_empty(), "tenant B recalled tenant A's episode");
    assert_ne!(a.tenant(), b.tenant());
}

// ---- Retention and the learned-remedy gate ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_deletes_expired_entries_and_keeps_the_rest() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };
    let Some(memory) = fixture.memory("ret", Vec::new(), RetentionPolicy::default(), None) else {
        return;
    };
    let tenant = memory.tenant().as_str().to_owned();
    let memory = Arc::new(memory);
    let old = Entry::Episode(episode_saying(
        &tenant,
        1,
        "the running job was unstuck",
        1_000,
    ));
    let fresh_ms = 100 * DAY_MS;
    let fresh = Entry::Episode(episode_saying(
        &tenant,
        2,
        "another job was unstuck",
        fresh_ms,
    ));
    let work = Arc::clone(&memory);
    let report = tokio::task::spawn_blocking(move || {
        use spot_agent_memory::MemoryStore;
        work.store().put(old).unwrap();
        work.store().put(fresh).unwrap();
        // Episodes live 90 days by default: at day 100 the first is expired, the second is not.
        let report = work.run_retention(fresh_ms + DAY_MS).unwrap();
        let left = work.store().count(MemoryKind::Episode).unwrap();
        (report, left)
    })
    .await
    .unwrap();
    assert_eq!(report.0.expired.len(), 1);
    assert_eq!(report.1, 1);
    // Applying the same pass again changes nothing.
    let again = Arc::clone(&memory);
    let second =
        tokio::task::spawn_blocking(move || again.run_retention(fresh_ms + DAY_MS).unwrap())
            .await
            .unwrap();
    assert!(second.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hold_file_that_cannot_be_read_stops_retention_instead_of_deleting() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };
    let missing = std::env::temp_dir().join(format!("spot_no_holds_{}", RecordId::new_v7()));
    let Some(memory) = fixture.memory(
        "hold",
        Vec::new(),
        RetentionPolicy::default(),
        Some(missing),
    ) else {
        return;
    };
    let tenant = memory.tenant().as_str().to_owned();
    let memory = Arc::new(memory);
    let work = Arc::clone(&memory);
    let (outcome, left) = tokio::task::spawn_blocking(move || {
        use spot_agent_memory::MemoryStore;
        work.store()
            .put(Entry::Episode(episode_saying(
                &tenant,
                1,
                "the running job was unstuck",
                1_000,
            )))
            .unwrap();
        let outcome = work.run_retention(1_000 + 200 * DAY_MS);
        (outcome, work.store().count(MemoryKind::Episode).unwrap())
    })
    .await
    .unwrap();
    assert_eq!(outcome.unwrap_err(), MemoryFault::Holds);
    assert_eq!(left, 1, "nothing was deleted without sight of the holds");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_learned_remedy_stays_blocked_until_its_ci_flag_is_set() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };
    let Some(memory) = fixture.memory("ci", Vec::new(), RetentionPolicy::default(), None) else {
        return;
    };
    let tenant = memory.tenant().as_str().to_owned();
    let memory = Arc::new(memory);
    let work = Arc::clone(&memory);
    tokio::task::spawn_blocking(move || {
        let mut candidate =
            propose_remedy(&procedure(&tenant, Tier::T2), &LearningPolicy::default()).unwrap();
        let id: CandidateId = candidate.id().clone();
        work.record_candidate(&candidate).unwrap();
        assert!(!work.remedy_usable(&id).unwrap(), "proposed is not usable");
        assert!(
            !work
                .remedy_usable(&CandidateId::parse("candidate.absent").unwrap())
                .unwrap()
        );

        let ci = CiPromotion::new();
        let run = CiRunId::parse("ci-run-42").unwrap();
        ci.start(&mut candidate, run.clone()).unwrap();
        work.record_candidate(&candidate).unwrap();
        assert!(!work.remedy_usable(&id).unwrap(), "in CI is not usable");

        // CI has not reported: it cannot settle, so it cannot be promoted.
        assert!(ci.settle(&mut candidate).is_err());
        ci.report(run.clone(), CiOutcome::Passed);
        ci.settle(&mut candidate).unwrap();
        work.record_candidate(&candidate).unwrap();
        assert!(
            !work.remedy_usable(&id).unwrap(),
            "passed is not yet promoted"
        );

        let usable = ci.promote(candidate, run).unwrap();
        work.record_candidate(usable.candidate()).unwrap();
        assert!(
            work.remedy_usable(&id).unwrap(),
            "promoted with its CI run is usable"
        );
    })
    .await
    .unwrap();
}

#[test]
fn the_site_name_is_the_tenant() {
    let site = SiteId::new("acme-ltd").unwrap();
    assert_eq!(
        spot_cli::services::agent_memory::host::tenant_of(&site)
            .unwrap()
            .as_str(),
        "acme-ltd"
    );
}
