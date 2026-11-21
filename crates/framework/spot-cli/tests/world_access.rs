//! WP-AI-07.T5: the world-access composer. The composer is built over the in-memory fixture parts
//! (`world_access::fixture`) or over no parts (what the stub part files give a real site), and the
//! remedy host runs on it, so the tests prove the composer's rules without a site or a database:
//! live and rehearsal worlds, consistent independent snapshots, a missing part degrading only the
//! remedies that read it, and the host no longer answering `remedies_host.no_world`.
//!
//! The fix-pipeline rig (`fix_pipeline_service.rs`) is another task's file, so no test here
//! installs the pipeline against the composer; the host-level apply path is covered instead.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;
use spot_agent_tiers::{AgentIdentity, AgentRegistry, Domain, Status, Tier};
use spot_cli::services::remedies::host::{AgentSource, Deps, Fut, HostError, RemedyHost};
use spot_cli::services::remedies::world::{
    CtxRequest, ShippedCatalogue, WorldAccess, WorldError, WorldKind,
};
use spot_cli::services::world_access::fixture::FixtureWorld;
use spot_cli::services::world_access::parts::JobsPart;
use spot_cli::services::world_access::unavailable::NOT_AVAILABLE;
use spot_cli::services::world_access::{Parts, Settings, WorldComposer};
use spot_core::ManualClock;
use spot_mcp::AuditSink;
use spot_remedies::fixture::FakeJobs;
use spot_remedies::ports::{IndexHealth, JobLife};
use spot_remedies::{
    CacheNs, FakeWorld, IndexRef, JobRef, MemoryAudit, RemedyCtx, RemedyError, RemedyId,
    RemedySettings,
};
use time::OffsetDateTime;

fn id(text: &str) -> RemedyId {
    RemedyId::new(text)
}

fn seeded() -> FakeWorld {
    let mut world = FakeWorld::new();
    world.jobs.seed("j1", JobLife::Failed, 1, None);
    world.cache.seed("sessions", 5);
    world.index.seed("retrieval", IndexHealth::Stale, 3);
    world
}

fn composer_over(world: &FixtureWorld) -> Arc<WorldComposer> {
    Arc::new(WorldComposer::with_parts(
        Settings::default(),
        world.parts(),
    ))
}

/// Opens a world of `kind` on the composer and runs `work` once.
fn run_on(
    composer: &WorldComposer,
    kind: WorldKind,
    work: impl FnMut(&mut RemedyCtx<'_>),
) -> Result<(), WorldError> {
    let settings = RemedySettings::default();
    let mut audit = MemoryAudit::new();
    let mut work = work;
    composer.with_ctx(
        kind,
        CtxRequest {
            actor: "agent-t1",
            now: 1_000,
            settings: &settings,
            audit: &mut audit,
        },
        &mut work,
    )
}

fn job_life(ctx: &mut RemedyCtx<'_>, job: &str) -> Option<JobLife> {
    ctx.jobs().job(&JobRef::new(job)).map(|view| view.life)
}

struct Agents(HashMap<String, AgentIdentity>);

impl Agents {
    fn standard() -> Arc<Self> {
        let mut map = HashMap::new();
        let mut add = |name: &str, tier: Tier| {
            let mut registry = AgentRegistry::new();
            registry
                .enroll(
                    AgentIdentity::new(name, "owner", Domain::Support, tier)
                        .with_status(Status::Active),
                )
                .unwrap();
            map.insert(name.to_owned(), registry.get(name).cloned().unwrap());
        };
        add("agent-t1", Tier::T1);
        add("agent-t2", Tier::T2);
        Arc::new(Self(map))
    }
}

impl AgentSource for Agents {
    fn identity<'a>(
        &'a self,
        tenant: &'a str,
        agent_id: &'a str,
    ) -> Fut<'a, Option<AgentIdentity>> {
        Box::pin(async move {
            if tenant != "site-1" {
                return None;
            }
            self.0.get(agent_id).cloned()
        })
    }
}

fn host_over(composer: Arc<WorldComposer>) -> RemedyHost {
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_795_262_400).unwrap(),
    ));
    let sink: Arc<dyn AuditSink> = Arc::new(spot_mcp::MemoryAudit::new());
    RemedyHost::new(Deps {
        site: "site-1".to_owned(),
        catalogue: Arc::new(ShippedCatalogue),
        settings: RemedySettings::default(),
        world: Some(composer as Arc<dyn WorldAccess>),
        agents: Agents::standard(),
        apply_path: None,
        sink,
        clock: clock as Arc<dyn spot_core::Clock>,
    })
    .unwrap()
}

#[tokio::test]
async fn with_no_parts_the_host_lists_self_tests_and_refuses_cleanly() {
    let composer = Arc::new(WorldComposer::with_parts(
        Settings::default(),
        Parts::none(),
    ));
    let host = host_over(composer);
    assert!(
        host.has_world(),
        "a composer is a world, even with no parts"
    );

    let listed = host.list("site-1", "agent-t1").await.unwrap();
    assert!(!listed.is_empty());
    for result in host.self_test_all().unwrap() {
        assert!(
            result.outcome.is_ok(),
            "{}: {:?}",
            result.remedy.as_str(),
            result.outcome
        );
    }

    // A jobs remedy over the missing jobs part is a clean refusal of the remedy framework, never
    // the host's `no_world`.
    let refused = host
        .dry_run(
            "site-1",
            "agent-t1",
            &id(concat!("job", ".retry_failed")),
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused,
            HostError::Remedy(RemedyError::PreconditionFailed { .. })
        ),
        "{refused}"
    );
    assert_ne!(refused.code(), "remedies_host.no_world");
}

#[tokio::test]
async fn a_t1_remedy_dry_runs_applies_and_undoes_on_the_live_world() {
    let world = FixtureWorld::new(seeded());
    let host = host_over(composer_over(&world));
    let before = world.fingerprint();
    let remedy = id(concat!("job", ".retry_failed"));

    let plan = host
        .dry_run(
            "site-1",
            "agent-t1",
            &remedy,
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap();
    assert!(plan.verify());
    assert_eq!(world.fingerprint(), before, "a dry-run changes nothing");

    let applied = host.apply("site-1", "agent-t1", &plan, None).await.unwrap();
    assert_eq!(world.job_life("j1"), Some(JobLife::Queued));
    assert_ne!(world.fingerprint(), before);

    host.undo("site-1", "agent-t1", &remedy, &applied.undo_token, None)
        .await
        .unwrap();
    assert_eq!(
        world.fingerprint(),
        before,
        "undo leaves the world byte-identical"
    );
}

#[test]
fn a_rehearsal_apply_changes_only_the_snapshot() {
    let world = FixtureWorld::new(seeded());
    let composer = composer_over(&world);
    let before = world.fingerprint();

    let mut after = None;
    run_on(&composer, WorldKind::Rehearsal, |ctx| {
        let _ = ctx.jobs().retry(&JobRef::new("j1"), 1_000);
        let _ = ctx.cache().flush(&CacheNs::new("sessions"));
        let _ = ctx.index().rebuild(&IndexRef::new("retrieval"));
        after = job_life(ctx, "j1");
    })
    .unwrap();

    assert_eq!(after, Some(JobLife::Queued), "the snapshot changed");
    assert_eq!(world.fingerprint(), before, "the live world did not");
    assert_eq!(world.job_life("j1"), Some(JobLife::Failed));
}

#[test]
fn two_rehearsals_are_independent_and_see_the_live_world_as_it_is() {
    let world = FixtureWorld::new(seeded());
    let composer = composer_over(&world);

    run_on(&composer, WorldKind::Rehearsal, |ctx| {
        let _ = ctx.jobs().retry(&JobRef::new("j1"), 1_000);
    })
    .unwrap();

    let mut second = None;
    run_on(&composer, WorldKind::Rehearsal, |ctx| {
        second = job_life(ctx, "j1");
    })
    .unwrap();
    assert_eq!(
        second,
        Some(JobLife::Failed),
        "the first rehearsal left nothing behind"
    );

    // A change on the live world shows in the next snapshot.
    run_on(&composer, WorldKind::Live, |ctx| {
        let _ = ctx.jobs().retry(&JobRef::new("j1"), 1_000);
    })
    .unwrap();
    let mut third = None;
    run_on(&composer, WorldKind::Rehearsal, |ctx| {
        third = job_life(ctx, "j1");
    })
    .unwrap();
    assert_eq!(third, Some(JobLife::Queued));
}

#[test]
fn a_missing_part_degrades_only_the_remedies_that_read_it() {
    let world = FixtureWorld::new(seeded());
    let mut parts = world.parts();
    parts.jobs = None;
    let composer = Arc::new(WorldComposer::with_parts(Settings::default(), parts));
    assert_eq!(composer.degraded().as_deref(), Some("no jobs"));

    let mut results = Vec::new();
    run_on(&composer, WorldKind::Live, |ctx| {
        results.push(ctx.jobs().retry(&JobRef::new("j1"), 1_000).map(|_| ()));
        results.push(ctx.cache().flush(&CacheNs::new("sessions")).map(|_| ()));
        assert!(ctx.jobs().failed().is_empty(), "reads answer empty");
    })
    .unwrap();
    let jobs = results[0].clone().unwrap_err();
    assert_eq!(jobs.detail, NOT_AVAILABLE);
    assert!(results[1].is_ok(), "the cache part is live");
}

#[tokio::test]
async fn a_missing_part_refuses_its_remedy_and_leaves_the_others_running() {
    let world = FixtureWorld::new(seeded());
    let mut parts = world.parts();
    parts.jobs = None;
    let host = host_over(Arc::new(WorldComposer::with_parts(
        Settings::default(),
        parts,
    )));

    let refused = host
        .dry_run(
            "site-1",
            "agent-t1",
            &id(concat!("job", ".retry_failed")),
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        refused,
        HostError::Remedy(RemedyError::PreconditionFailed { .. })
    ));

    let plan = host
        .dry_run(
            "site-1",
            "agent-t1",
            &id(concat!("cache", ".flush_namespace")),
            &json!({"namespace": "sessions"}),
            None,
        )
        .await
        .unwrap();
    assert!(plan.verify());
}

#[test]
fn rehearsal_can_be_switched_off_and_live_still_runs() {
    let world = FixtureWorld::new(seeded());
    let composer = WorldComposer::with_parts(
        Settings {
            enabled: true,
            rehearsal: false,
        },
        world.parts(),
    );
    let refused = run_on(&composer, WorldKind::Rehearsal, |_| {
        panic!("no world was opened")
    })
    .unwrap_err();
    assert!(refused.to_string().contains("rehearsal_off"), "{refused}");
    run_on(&composer, WorldKind::Live, |ctx| {
        assert_eq!(job_life(ctx, "j1"), Some(JobLife::Failed));
    })
    .unwrap();
}

#[test]
fn an_unfilled_composer_opens_no_world() {
    let composer = WorldComposer::new(Settings::default());
    for kind in [WorldKind::Live, WorldKind::Rehearsal] {
        let refused = run_on(&composer, kind, |_| panic!("no world was opened")).unwrap_err();
        assert!(refused.to_string().contains("not_assembled"), "{refused}");
    }
}

#[test]
fn the_fixture_parts_all_exist_and_name_nothing_missing() {
    let world = FixtureWorld::new(FakeWorld::new());
    assert!(world.parts().missing().is_empty());
    assert_eq!(Parts::none().missing().len(), 7);
}

/// A jobs part that records who a live world was opened for.
struct ActorProbe(Mutex<Vec<String>>);

impl JobsPart for ActorProbe {
    fn live(&self) -> Box<dyn spot_remedies::JobOps + '_> {
        Box::new(FakeJobs::new())
    }

    fn live_as(&self, actor: &str) -> Box<dyn spot_remedies::JobOps + '_> {
        self.0.lock().unwrap().push(actor.to_owned());
        Box::new(FakeJobs::new())
    }

    fn rehearsal(&self) -> Result<Box<dyn spot_remedies::JobOps>, WorldError> {
        Ok(Box::new(FakeJobs::new()))
    }
}

#[test]
fn a_live_world_is_opened_for_the_remedys_own_actor() {
    let probe = Arc::new(ActorProbe(Mutex::new(Vec::new())));
    let parts = Parts {
        jobs: Some(Arc::clone(&probe) as Arc<dyn JobsPart>),
        ..Parts::none()
    };
    let composer = WorldComposer::with_parts(Settings::default(), parts);
    run_on(&composer, WorldKind::Live, |_| {}).unwrap();
    run_on(&composer, WorldKind::Rehearsal, |_| {}).unwrap();
    assert_eq!(*probe.0.lock().unwrap(), vec!["agent-t1".to_owned()]);
}
