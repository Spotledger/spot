//! WP-AI-07.T2: the remedy catalogue hosted. The host runs over the framework's in-memory world
//! (one copy live, one a rehearsal snapshot), an in-memory audit chain and stub agent registrations,
//! so the tests prove the host's rules (tier filtering, mandatory dry-run, the fix-pipeline route,
//! audited apply and undo, tenant scoping, fail-closed audit) without a database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use spot_agent_tiers::{AgentIdentity, AgentRegistry, Domain, Status, Tier};
use spot_audit_chain::{AuditRecord, PayloadValue};
use spot_cli::services::remedies::host::{
    AgentSource, ApplyPath, Deps, Fut, HostError, PipelineRun, RemedyHost,
};
use spot_cli::services::remedies::world::{
    CtxRequest, ShippedCatalogue, WorldAccess, WorldError, WorldKind,
};
use spot_core::ManualClock;
use spot_mcp::{AuditSink, McpError};
use spot_remedies::ports::{IndexHealth, JobLife};
use spot_remedies::{Applied, FakeWorld, RemedyCtx, RemedyError, RemedyId, RemedySettings};
use time::OffsetDateTime;

fn id(text: &str) -> RemedyId {
    RemedyId::new(text)
}

/// The framework's in-memory world, once live and once as the rehearsal snapshot.
struct Worlds {
    live: Mutex<FakeWorld>,
    rehearsal: Mutex<FakeWorld>,
    opened: AtomicUsize,
}

impl Worlds {
    fn seeded() -> Arc<Self> {
        let seed = || {
            let mut world = FakeWorld::new();
            world.jobs.seed("j1", JobLife::Failed, 1, None);
            world.index.seed("retrieval", IndexHealth::Stale, 3);
            world
        };
        Arc::new(Self {
            live: Mutex::new(seed()),
            rehearsal: Mutex::new(seed()),
            opened: AtomicUsize::new(0),
        })
    }

    /// The whole live world as text, for the "nothing changed" comparisons.
    fn live_state(&self) -> String {
        format!("{:?}", *self.live.lock().unwrap())
    }

    fn rehearsal_state(&self) -> String {
        format!("{:?}", *self.rehearsal.lock().unwrap())
    }
}

impl WorldAccess for Worlds {
    fn with_ctx(
        &self,
        kind: WorldKind,
        request: CtxRequest<'_>,
        work: &mut dyn FnMut(&mut RemedyCtx<'_>),
    ) -> Result<(), WorldError> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        let cell = match kind {
            WorldKind::Live => &self.live,
            WorldKind::Rehearsal => &self.rehearsal,
        };
        let mut guard = cell.lock().unwrap();
        let world = &mut *guard;
        let mut ctx = RemedyCtx::new(
            &mut world.jobs,
            &mut world.cache,
            &mut world.index,
            &mut world.edge,
            &mut world.quarantine,
            &mut world.case,
            &mut world.document,
            request.audit,
            request.actor,
            request.now,
            request.settings,
        );
        work(&mut ctx);
        Ok(())
    }
}

/// Stored registrations, as the tier service would answer them.
struct Agents(HashMap<String, AgentIdentity>);

impl Agents {
    fn standard() -> Arc<Self> {
        let mut map = HashMap::new();
        let mut add = |name: &str, domain: Domain, tier: Tier, status: Status| {
            let mut registry = AgentRegistry::new();
            registry
                .enroll(AgentIdentity::new(name, "owner", domain, tier).with_status(status))
                .unwrap();
            map.insert(name.to_owned(), registry.get(name).cloned().unwrap());
        };
        add("agent-t0", Domain::Support, Tier::T0, Status::Active);
        add("agent-t1", Domain::Support, Tier::T1, Status::Active);
        add("agent-t2", Domain::Support, Tier::T2, Status::Active);
        add("agent-ops", Domain::Operations, Tier::T2, Status::Active);
        add("agent-off", Domain::Support, Tier::T2, Status::Suspended);
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

/// The audit chain stand-in that refuses every record.
struct Failing;

impl AuditSink for Failing {
    fn record<'a>(&'a self, _record: AuditRecord) -> spot_mcp::BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async { Err(McpError::internal("the chain is down")) })
    }
}

/// The fix pipeline stand-in: rehearse on the snapshot, then apply live.
struct Pipeline {
    steps: Mutex<Vec<&'static str>>,
}

impl ApplyPath for Pipeline {
    fn apply<'a>(&'a self, run: PipelineRun<'a>) -> Fut<'a, Result<Applied, HostError>> {
        Box::pin(async move {
            run.direct.apply(WorldKind::Rehearsal).await?;
            self.steps.lock().unwrap().push("rehearsed");
            let applied = run.direct.apply(WorldKind::Live).await?;
            self.steps.lock().unwrap().push("applied live");
            Ok(applied)
        })
    }
}

struct Rig {
    host: RemedyHost,
    worlds: Arc<Worlds>,
    audit: Arc<spot_mcp::MemoryAudit>,
    clock: Arc<ManualClock>,
    pipeline: Option<Arc<Pipeline>>,
}

fn rig_with(world: bool, pipeline: bool, failing_audit: bool) -> Rig {
    let worlds = Worlds::seeded();
    let audit = Arc::new(spot_mcp::MemoryAudit::new());
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_795_262_400).unwrap(),
    ));
    let pipeline = pipeline.then(|| {
        Arc::new(Pipeline {
            steps: Mutex::new(Vec::new()),
        })
    });
    let sink: Arc<dyn AuditSink> = if failing_audit {
        Arc::new(Failing)
    } else {
        Arc::clone(&audit) as Arc<dyn AuditSink>
    };
    let host = RemedyHost::new(Deps {
        site: "site-1".to_owned(),
        catalogue: Arc::new(ShippedCatalogue),
        settings: RemedySettings::default(),
        world: world.then(|| Arc::clone(&worlds) as Arc<dyn WorldAccess>),
        agents: Agents::standard(),
        apply_path: pipeline
            .as_ref()
            .map(|p| Arc::clone(p) as Arc<dyn ApplyPath>),
        sink,
        clock: Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
    })
    .unwrap();
    Rig {
        host,
        worlds,
        audit,
        clock,
        pipeline,
    }
}

fn rig() -> Rig {
    rig_with(true, false, false)
}

fn kinds(audit: &spot_mcp::MemoryAudit) -> Vec<String> {
    audit.records().into_iter().map(|r| r.kind).collect()
}

fn text_of(record: &AuditRecord, key: &str) -> String {
    match record.payload.get(key) {
        Some(PayloadValue::Text(text)) => text.clone(),
        _ => String::new(),
    }
}

#[tokio::test]
async fn every_shipped_remedy_passes_its_self_test_and_undo() {
    let r = rig();
    let results = r.host.self_test_all().unwrap();
    assert!(
        results.len() >= 7,
        "the shipped catalogue has at least seven remedies"
    );
    for result in &results {
        assert!(
            result.outcome.is_ok(),
            "{}: {:?}",
            result.remedy.as_str(),
            result.outcome
        );
    }
    // The self-test runs on a fresh in-memory world: the live site was never opened.
    assert_eq!(r.worlds.opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn listing_is_filtered_by_tier_domain_and_status() {
    let r = rig();
    let t0 = r.host.list("site-1", "agent-t0").await.unwrap();
    assert!(t0.is_empty(), "T0 explains and reads; no remedy is T0");
    let t1 = r.host.list("site-1", "agent-t1").await.unwrap();
    assert!(!t1.is_empty());
    assert!(t1.iter().all(|s| s.tier <= Tier::T1));
    let t2 = r.host.list("site-1", "agent-t2").await.unwrap();
    assert!(t2.len() > t1.len());
    assert!(t2.iter().any(|s| s.tier == Tier::T2));
    assert!(
        t2.iter().all(|s| s.tier <= Tier::T2),
        "T3 stays out of a T2 listing"
    );
    assert!(r.host.list("site-1", "agent-ops").await.unwrap().is_empty());
    assert!(r.host.list("site-1", "agent-off").await.unwrap().is_empty());
    assert_eq!(
        r.host.list("site-1", "nobody").await.unwrap_err(),
        HostError::NotAnAgent
    );
}

#[tokio::test]
async fn a_dry_run_changes_nothing() {
    let r = rig();
    let before = r.worlds.live_state();
    let plan = r
        .host
        .dry_run(
            "site-1",
            "agent-t1",
            &id(concat!("job", ".retry_failed")),
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap();
    assert!(plan.verify());
    assert_eq!(
        r.worlds.live_state(),
        before,
        "the live world is byte-for-byte the same"
    );
    assert_eq!(
        r.worlds.rehearsal_state(),
        Worlds::seeded().rehearsal_state()
    );
    let k = kinds(&r.audit);
    assert!(k.iter().any(|k| k.ends_with(".planned")), "{k:?}");
}

#[tokio::test]
async fn a_reversible_t1_remedy_applies_directly_and_undo_restores_exactly() {
    let r = rig();
    let before = r.worlds.live_state();
    let remedy = id(concat!("job", ".retry_failed"));
    let plan = r
        .host
        .dry_run(
            "site-1",
            "agent-t1",
            &remedy,
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap();
    let applied = r
        .host
        .apply("site-1", "agent-t1", &plan, None)
        .await
        .unwrap();
    assert_eq!(
        r.worlds.live.lock().unwrap().job_life("j1"),
        Some(JobLife::Queued)
    );
    assert_ne!(r.worlds.live_state(), before);

    let undone = r
        .host
        .undo("site-1", "agent-t1", &remedy, &applied.undo_token, None)
        .await
        .unwrap();
    assert_eq!(undone.token, applied.undo_token);
    assert_eq!(
        r.worlds.live_state(),
        before,
        "undo restores the pre-state exactly"
    );

    // The intent record precedes the world change, and every step is on the chain.
    let k = kinds(&r.audit);
    let pos = |suffix: &str| k.iter().position(|k| k.ends_with(suffix)).unwrap();
    assert!(pos(".apply_started") < pos(".applied"), "{k:?}");
    assert!(pos(".applied") < pos(".undo_started"), "{k:?}");
    assert!(k.iter().any(|k| k.ends_with(".undone")), "{k:?}");
    // The records name the agent and the remedy, never the parameters.
    for record in r.audit.records() {
        assert_eq!(record.actor, "agent:agent-t1");
        assert_eq!(text_of(&record, "remedy"), remedy.as_str());
        assert!(
            !format!("{record:?}").contains("j1"),
            "a parameter reached the chain"
        );
    }
}

#[tokio::test]
async fn a_plan_the_host_did_not_issue_is_refused() {
    let r = rig();
    let remedy = id(concat!("job", ".retry_failed"));
    let plan = r
        .host
        .dry_run(
            "site-1",
            "agent-t1",
            &remedy,
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap();
    r.host
        .apply("site-1", "agent-t1", &plan, None)
        .await
        .unwrap();
    // One dry-run authorises one apply: the same plan again is stale.
    let again = r
        .host
        .apply("site-1", "agent-t1", &plan, None)
        .await
        .unwrap_err();
    assert!(
        matches!(again, HostError::Remedy(RemedyError::PlanStale { .. })),
        "{again}"
    );
}

#[tokio::test]
async fn a_remedy_above_the_agents_tier_is_refused_and_audited() {
    let r = rig();
    let before = r.worlds.live_state();
    let refused = r
        .host
        .dry_run(
            "site-1",
            "agent-t1",
            &id(concat!("index", ".rebuild_retrieval")),
            &json!({"index": "retrieval"}),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refused, HostError::Remedy(RemedyError::Refused { .. })),
        "{refused}"
    );
    assert_eq!(r.worlds.live_state(), before);
    assert!(kinds(&r.audit).iter().any(|k| k.ends_with(".refused")));
}

#[tokio::test]
async fn a_t2_remedy_cannot_apply_until_the_fix_pipeline_is_installed() {
    let r = rig();
    let remedy = id(concat!("index", ".rebuild_retrieval"));
    let plan = r
        .host
        .dry_run(
            "site-1",
            "agent-t2",
            &remedy,
            &json!({"index": "retrieval"}),
            None,
        )
        .await
        .unwrap();
    let before = r.worlds.live_state();
    let refused = r
        .host
        .apply("site-1", "agent-t2", &plan, None)
        .await
        .unwrap_err();
    assert_eq!(refused, HostError::ApplyClosed);
    assert_eq!(refused.code(), "remedies_host.apply_closed");
    assert_eq!(r.worlds.live_state(), before);
    assert!(kinds(&r.audit).iter().any(|k| k.ends_with(".apply_closed")));
}

#[tokio::test]
async fn a_t2_remedy_applies_through_the_pipeline_after_a_rehearsal() {
    let r = rig_with(true, true, false);
    let remedy = id(concat!("index", ".rebuild_retrieval"));
    let plan = r
        .host
        .dry_run(
            "site-1",
            "agent-t2",
            &remedy,
            &json!({"index": "retrieval"}),
            None,
        )
        .await
        .unwrap();
    let rehearsal_before = r.worlds.rehearsal_state();
    let live_before = r.worlds.live_state();
    r.host
        .apply("site-1", "agent-t2", &plan, None)
        .await
        .unwrap();

    let steps = r.pipeline.as_ref().unwrap().steps.lock().unwrap().clone();
    assert_eq!(steps, ["rehearsed", "applied live"]);
    assert_ne!(
        r.worlds.rehearsal_state(),
        rehearsal_before,
        "the snapshot was changed first"
    );
    assert_ne!(r.worlds.live_state(), live_before);
    assert_eq!(
        r.worlds.live.lock().unwrap().index_health("retrieval"),
        Some(IndexHealth::Fresh)
    );
}

#[tokio::test]
async fn undo_is_time_boxed() {
    let r = rig();
    let remedy = id(concat!("job", ".retry_failed"));
    let plan = r
        .host
        .dry_run(
            "site-1",
            "agent-t1",
            &remedy,
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap();
    let applied = r
        .host
        .apply("site-1", "agent-t1", &plan, None)
        .await
        .unwrap();
    r.clock.advance(Duration::from_secs(86_400 + 1));
    let late = r
        .host
        .undo("site-1", "agent-t1", &remedy, &applied.undo_token, None)
        .await
        .unwrap_err();
    assert!(
        matches!(late, HostError::Remedy(RemedyError::UndoExpired { .. })),
        "{late}"
    );
    assert!(kinds(&r.audit).iter().any(|k| k.ends_with(".undo_refused")));
    assert_eq!(
        r.worlds.live.lock().unwrap().job_life("j1"),
        Some(JobLife::Queued),
        "the change stands"
    );
}

#[tokio::test]
async fn another_tenant_and_a_stranger_are_refused_and_audited() {
    let r = rig();
    let remedy = id(concat!("job", ".retry_failed"));
    let params = json!({"job_id": "j1"});
    assert_eq!(
        r.host
            .dry_run("site-2", "agent-t1", &remedy, &params, None)
            .await
            .unwrap_err(),
        HostError::WrongTenant
    );
    assert_eq!(
        r.host
            .dry_run("site-1", "stranger", &remedy, &params, None)
            .await
            .unwrap_err(),
        HostError::NotAnAgent
    );
    assert_eq!(
        kinds(&r.audit)
            .iter()
            .filter(|k| k.ends_with(".call_refused"))
            .count(),
        2
    );
    assert_eq!(
        r.worlds.opened.load(Ordering::SeqCst),
        0,
        "no world was opened"
    );
}

#[tokio::test]
async fn nothing_runs_when_the_audit_chain_cannot_take_the_intent() {
    let r = rig_with(true, false, true);
    let before = r.worlds.live_state();
    let remedy = id(concat!("job", ".retry_failed"));
    // A plan is read-only, but its records could not be kept: the result is withheld.
    let denied = r
        .host
        .dry_run(
            "site-1",
            "agent-t1",
            &remedy,
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(denied, HostError::AuditUnavailable);
    assert_eq!(r.worlds.live_state(), before);

    // An apply never reaches the world: the intent record comes first.
    let healthy = rig();
    let plan = healthy
        .host
        .dry_run(
            "site-1",
            "agent-t1",
            &remedy,
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap();
    let failing = rig_with(true, false, true);
    let before = failing.worlds.live_state();
    let denied = failing
        .host
        .apply("site-1", "agent-t1", &plan, None)
        .await
        .unwrap_err();
    assert_eq!(denied, HostError::AuditUnavailable);
    assert_eq!(failing.worlds.live_state(), before);
}

#[tokio::test]
async fn without_a_world_the_host_lists_but_cannot_run() {
    let r = rig_with(false, false, false);
    assert!(!r.host.has_world());
    assert!(!r.host.list("site-1", "agent-t1").await.unwrap().is_empty());
    let denied = r
        .host
        .dry_run(
            "site-1",
            "agent-t1",
            &id(concat!("job", ".retry_failed")),
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(denied, HostError::NoWorld);
    assert_eq!(denied.code(), "remedies_host.no_world");
}

#[tokio::test]
async fn parameters_are_validated_against_the_remedys_schema() {
    let r = rig();
    let bad = r
        .host
        .dry_run(
            "site-1",
            "agent-t1",
            &id(concat!("job", ".retry_failed")),
            &json!({"job_id": 5}),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(bad, HostError::Remedy(RemedyError::ParamsInvalid { .. })),
        "{bad}"
    );
    let unknown = r
        .host
        .dry_run(
            "site-1",
            "agent-t1",
            &id(concat!("no", ".such_remedy")),
            &json!({}),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(unknown, HostError::Remedy(RemedyError::Unknown { .. })),
        "{unknown}"
    );
}

#[tokio::test]
async fn the_pipeline_can_be_installed_after_the_host_is_built() {
    // Ports flow forward only, so the fix pipeline (which registers later) installs itself.
    let r = rig();
    assert!(!r.host.has_pipeline());
    let remedy = id(concat!("index", ".rebuild_retrieval"));
    let plan = r
        .host
        .dry_run(
            "site-1",
            "agent-t2",
            &remedy,
            &json!({"index": "retrieval"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        r.host
            .apply("site-1", "agent-t2", &plan, None)
            .await
            .unwrap_err(),
        HostError::ApplyClosed
    );
    r.host.install_apply_path(Arc::new(Pipeline {
        steps: Mutex::new(Vec::new()),
    }));
    assert!(r.host.has_pipeline());
    r.host
        .apply("site-1", "agent-t2", &plan, None)
        .await
        .unwrap();
    assert_eq!(
        r.worlds.live.lock().unwrap().index_health("retrieval"),
        Some(IndexHealth::Fresh)
    );
    assert!(r.host.meta(&remedy).is_some());
    assert!(r.host.world().is_some());
}
