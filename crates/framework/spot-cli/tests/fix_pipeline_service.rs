//! WP-AI-08.T4: the fix pipeline hosted. The pipeline runs over the framework's in-memory world
//! (one copy live, one the rehearsal snapshot), the in-memory journal, an in-memory audit chain and
//! a recording effect hold, through the remedy host it installs itself on. The properties proved:
//! a rehearsal never touches the live site, the hold is on before the first live change and
//! released only for a verified fix, a failed verification leaves the world byte-identical, one run
//! per site, undo inside the window and a closed window after it, and fail-closed on a missing
//! hold or a refusing audit chain. The journal is the in-memory one here; the PostgreSQL journal
//! has its own crate tests.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use spot_agent_tiers::{AgentIdentity, AgentRegistry, Domain, Tier};
use spot_audit_chain::AuditRecord;
use spot_cli::services::fix_pipeline::host::{
    Deps as FixDeps, EffectHold, FixHost, NoSnapshots, PipelinePath,
};
use spot_cli::services::fix_pipeline::ports::{CheckOutcome, FixChecks};
use spot_cli::services::remedies::host::{AgentSource, Deps, Fut, HostError, RemedyHost};
use spot_cli::services::remedies::world::{
    CtxRequest, ShippedCatalogue, WorldAccess, WorldError, WorldKind,
};
use spot_core::ManualClock;
use spot_fix_pipeline::{
    AgentRef, ChangeRecord, DeploymentEpoch, FixId, FixJournal, FixPlan, FixPolicy, FixSource,
    FixState, FixStep, InMemoryJournal, JournalRecord, PlanPath, Risk,
};
use spot_mcp::{AuditSink, McpError};
use spot_remedies::ports::IndexHealth;
use spot_remedies::{FakeWorld, RemedyCtx, RemedyId, RemedySettings};
use time::OffsetDateTime;

fn index_remedy() -> RemedyId {
    RemedyId::new(concat!("index", ".rebuild_retrieval"))
}

struct Worlds {
    live: Mutex<FakeWorld>,
    rehearsal: Mutex<FakeWorld>,
}

impl Worlds {
    /// A stale retrieval index on the live site and, unless `rehearsal_clean`, on the snapshot.
    fn new(rehearsal_clean: bool) -> Arc<Self> {
        let seed = |health| {
            let mut world = FakeWorld::new();
            world.index.seed("retrieval", health, 3);
            world
        };
        Arc::new(Self {
            live: Mutex::new(seed(IndexHealth::Stale)),
            rehearsal: Mutex::new(seed(if rehearsal_clean {
                IndexHealth::Fresh
            } else {
                IndexHealth::Stale
            })),
        })
    }

    fn live_state(&self) -> String {
        format!("{:?}", *self.live.lock().unwrap())
    }

    fn live_health(&self) -> Option<IndexHealth> {
        self.live.lock().unwrap().index_health("retrieval")
    }

    fn rehearsal_health(&self) -> Option<IndexHealth> {
        self.rehearsal.lock().unwrap().index_health("retrieval")
    }
}

impl WorldAccess for Worlds {
    fn with_ctx(
        &self,
        kind: WorldKind,
        request: CtxRequest<'_>,
        work: &mut dyn FnMut(&mut RemedyCtx<'_>),
    ) -> Result<(), WorldError> {
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

struct Agents(HashMap<String, AgentIdentity>);

impl Agents {
    fn standard() -> Arc<Self> {
        let mut map = HashMap::new();
        let mut registry = AgentRegistry::new();
        registry
            .enroll(AgentIdentity::new(
                "agent-t2",
                "owner",
                Domain::Support,
                Tier::T2,
            ))
            .unwrap();
        map.insert(
            "agent-t2".to_owned(),
            registry.get("agent-t2").cloned().unwrap(),
        );
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

/// An audit chain that refuses every record.
struct Failing;

impl AuditSink for Failing {
    fn record<'a>(&'a self, _record: AuditRecord) -> spot_mcp::BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async { Err(McpError::internal("the chain is down")) })
    }
}

/// Records what the pipeline did to the hold, and the live index health at each call.
struct Hold {
    worlds: Arc<Worlds>,
    events: Mutex<Vec<(&'static str, Option<IndexHealth>)>>,
    started: AtomicBool,
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl Hold {
    fn events(&self) -> Vec<(&'static str, Option<IndexHealth>)> {
        self.events.lock().unwrap().clone()
    }

    fn note(&self, what: &'static str) {
        let health = self.worlds.live_health();
        self.events.lock().unwrap().push((what, health));
    }
}

impl EffectHold for Hold {
    fn begin<'a>(
        &'a self,
        _id: &'a str,
        _reason: &'a str,
        _now: i64,
    ) -> Fut<'a, Result<(), String>> {
        Box::pin(async move {
            self.note("begin");
            self.started.store(true, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            Ok(())
        })
    }

    fn release<'a>(&'a self, _id: &'a str, _now: i64) -> Fut<'a, Result<(), String>> {
        Box::pin(async move {
            self.note("release");
            Ok(())
        })
    }

    fn discard<'a>(
        &'a self,
        _id: &'a str,
        _reason: &'a str,
        _now: i64,
    ) -> Fut<'a, Result<(), String>> {
        Box::pin(async move {
            self.note("discard");
            Ok(())
        })
    }
}

struct Checks {
    symptom_cleared: AtomicBool,
}

impl FixChecks for Checks {
    fn check(&self, _plan: &FixPlan, _changes: &[ChangeRecord]) -> CheckOutcome {
        CheckOutcome {
            symptom_cleared: self.symptom_cleared.load(Ordering::SeqCst),
            violations: Vec::new(),
        }
    }
}

struct Rig {
    remedies: Arc<RemedyHost>,
    fix: Arc<FixHost>,
    worlds: Arc<Worlds>,
    journal: Arc<InMemoryJournal>,
    hold: Arc<Hold>,
    checks: Arc<Checks>,
    audit: Arc<spot_mcp::MemoryAudit>,
    clock: Arc<ManualClock>,
}

#[derive(Clone, Copy)]
struct Opts {
    rehearsal_clean: bool,
    with_hold: bool,
    failing_audit: bool,
    gate: bool,
}

const NORMAL: Opts = Opts {
    rehearsal_clean: false,
    with_hold: true,
    failing_audit: false,
    gate: false,
};

fn rig(opts: Opts) -> Rig {
    let worlds = Worlds::new(opts.rehearsal_clean);
    let audit = Arc::new(spot_mcp::MemoryAudit::new());
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_795_262_400).unwrap(),
    ));
    let sink: Arc<dyn AuditSink> = if opts.failing_audit {
        Arc::new(Failing)
    } else {
        Arc::clone(&audit) as Arc<dyn AuditSink>
    };
    let remedies = Arc::new(
        RemedyHost::new(Deps {
            site: "site-1".to_owned(),
            catalogue: Arc::new(ShippedCatalogue),
            settings: RemedySettings::default(),
            world: Some(Arc::clone(&worlds) as Arc<dyn WorldAccess>),
            agents: Agents::standard(),
            apply_path: None,
            sink: Arc::clone(&sink),
            clock: Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
        })
        .unwrap(),
    );
    let hold = Arc::new(Hold {
        worlds: Arc::clone(&worlds),
        events: Mutex::new(Vec::new()),
        started: AtomicBool::new(false),
        gate: opts.gate.then(|| Arc::new(tokio::sync::Notify::new())),
    });
    let checks = Arc::new(Checks {
        symptom_cleared: AtomicBool::new(true),
    });
    let journal = Arc::new(InMemoryJournal::new());
    let fix = Arc::new(FixHost::new(FixDeps {
        site: "site-1".to_owned(),
        remedies: Arc::clone(&remedies),
        journal: Arc::clone(&journal) as Arc<dyn FixJournal>,
        sink,
        hold: opts
            .with_hold
            .then(|| Arc::clone(&hold) as Arc<dyn EffectHold>),
        require_hold: true,
        snapshots: Arc::new(NoSnapshots),
        checks: Arc::clone(&checks) as Arc<dyn FixChecks>,
        policy: FixPolicy::default(),
        epoch: 1,
        clock: Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
    }));
    remedies.install_apply_path(Arc::new(PipelinePath(Arc::clone(&fix))));
    Rig {
        remedies,
        fix,
        worlds,
        journal,
        hold,
        checks,
        audit,
        clock,
    }
}

impl Rig {
    async fn plan(&self) -> spot_remedies::Plan {
        self.remedies
            .dry_run(
                "site-1",
                "agent-t2",
                &index_remedy(),
                &json!({"index": "retrieval"}),
                None,
            )
            .await
            .unwrap()
    }

    async fn apply(&self) -> Result<spot_remedies::Applied, HostError> {
        let plan = self.plan().await;
        self.remedies.apply("site-1", "agent-t2", &plan, None).await
    }

    /// The journal's record names for the (only) fix, in order.
    fn trail(&self) -> Vec<String> {
        self.journal
            .all()
            .unwrap()
            .iter()
            .map(|e| format!("{:?}", e.record.audit_action()))
            .collect()
    }

    fn fix_id(&self) -> FixId {
        self.journal.all().unwrap()[0].fix.clone()
    }
}

fn position(trail: &[String], name: &str) -> usize {
    trail
        .iter()
        .position(|t| t == name)
        .unwrap_or_else(|| panic!("{name} is not in {trail:?}"))
}

#[tokio::test]
async fn a_t2_remedy_is_rehearsed_applied_verified_and_journaled() {
    let r = rig(NORMAL);
    let applied = r.apply().await.unwrap();
    assert_eq!(r.worlds.live_health(), Some(IndexHealth::Fresh));
    assert_eq!(r.worlds.rehearsal_health(), Some(IndexHealth::Fresh));
    assert_eq!(applied.remedy, index_remedy());

    // The journal tells the whole story, in order, and the rehearsal came before the apply.
    let trail = r.trail();
    for pair in [
        ("Proposed", "Planned"),
        ("Planned", "RehearsalStarted"),
        ("Rehearsed", "ApplyStarted"),
        ("ApplyStarted", "PreImagesCaptured"),
        ("PreFingerprintCaptured", "StepStarted"),
        ("StepApplied", "Applied"),
        ("Applied", "VerifyStarted"),
        ("VerifyStarted", "Verified"),
        ("Verified", "UndoWindowOpened"),
    ] {
        assert!(
            position(&trail, pair.0) < position(&trail, pair.1),
            "{pair:?} in {trail:?}"
        );
    }
    let outcome = r.fix.status(&r.fix_id()).await.unwrap();
    assert_eq!(outcome.state, FixState::Verified);
    assert!(outcome.undo.is_some());

    // Every transition is on the audit chain, for this agent.
    let records = r.audit.records();
    let fix_records: Vec<_> = records
        .iter()
        .filter(|rec| rec.kind.starts_with("fix."))
        .collect();
    assert!(fix_records.len() >= 10);
    assert!(fix_records.iter().all(|rec| rec.actor == "agent:agent-t2"));
}

#[tokio::test]
async fn the_hold_is_on_before_the_first_live_change_and_released_when_verified() {
    let r = rig(NORMAL);
    r.apply().await.unwrap();
    let events = r.hold.events();
    assert_eq!(
        events,
        vec![
            // Taken while the live index was still stale: before the first live change.
            ("begin", Some(IndexHealth::Stale)),
            ("release", Some(IndexHealth::Fresh)),
        ]
    );
    assert_eq!(r.fix.degraded(), None);
}

#[tokio::test]
async fn a_failed_verification_rolls_back_with_no_residue_and_discards_the_hold() {
    let r = rig(NORMAL);
    r.checks.symptom_cleared.store(false, Ordering::SeqCst);
    let before = r.worlds.live_state();
    let refused = r.apply().await.unwrap_err();
    assert_eq!(refused.code(), "remedies.op_failed");
    assert!(refused.to_string().contains("rolled_back"), "{refused}");

    assert_eq!(
        r.worlds.live_state(),
        before,
        "the live site is byte-identical to its pre-state"
    );
    let trail = r.trail();
    assert!(position(&trail, "VerifyFailed") < position(&trail, "RollbackStarted"));
    assert!(position(&trail, "RollbackStarted") < position(&trail, "RolledBack"));
    assert!(!trail.contains(&"Verified".to_owned()));
    let outcome = r.fix.status(&r.fix_id()).await.unwrap();
    assert_eq!(outcome.state, FixState::RolledBack);
    // The residue the rollback measured is empty.
    assert!(
        outcome
            .residue
            .is_none_or(|residue| residue.differences.is_empty())
    );
    // The held effects are discarded, never released.
    let names: Vec<_> = r.hold.events().iter().map(|e| e.0).collect();
    assert_eq!(names, ["begin", "discard"]);
}

#[tokio::test]
async fn a_rehearsal_that_fails_never_touches_the_live_site() {
    // The snapshot has no fault, so the remedy cannot run on it.
    let r = rig(Opts {
        rehearsal_clean: true,
        ..NORMAL
    });
    let before = r.worlds.live_state();
    let refused = r.apply().await.unwrap_err();
    assert_eq!(refused.code(), "remedies.op_failed");
    assert_eq!(r.worlds.live_state(), before);
    assert_eq!(r.worlds.live_health(), Some(IndexHealth::Stale));
    assert!(
        r.hold.events().is_empty(),
        "the hold never went on: nothing live was attempted"
    );
    let trail = r.trail();
    assert!(trail.contains(&"RehearsalFailed".to_owned()), "{trail:?}");
    assert!(!trail.contains(&"ApplyStarted".to_owned()), "{trail:?}");
}

#[tokio::test]
async fn nothing_is_applied_without_the_effect_hold() {
    let r = rig(Opts {
        with_hold: false,
        ..NORMAL
    });
    let before = r.worlds.live_state();
    let refused = r.apply().await.unwrap_err();
    assert_eq!(refused.code(), "remedies.op_failed");
    assert_eq!(r.worlds.live_state(), before, "no hold, no live change");
}

#[tokio::test]
async fn a_refusing_audit_chain_stops_the_run_before_anything_changes() {
    let r = rig(Opts {
        failing_audit: true,
        ..NORMAL
    });
    let before = r.worlds.live_state();
    // The remedy host cannot keep the records of the plan, so no plan is handed out and nothing can
    // reach the pipeline.
    let refused = r
        .remedies
        .dry_run(
            "site-1",
            "agent-t2",
            &index_remedy(),
            &json!({"index": "retrieval"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(refused, HostError::AuditUnavailable);
    assert_eq!(r.worlds.live_state(), before);
    assert!(r.hold.events().is_empty());
    assert!(r.trail().is_empty());
}

#[tokio::test]
async fn a_second_concurrent_run_is_refused() {
    let r = rig(Opts {
        gate: true,
        ..NORMAL
    });
    let first_plan = r.plan().await;
    let second_plan = r.plan().await;
    let gate = Arc::clone(r.hold.gate.as_ref().unwrap());
    let first = r.remedies.apply("site-1", "agent-t2", &first_plan, None);
    let second = async {
        // Wait until the first run is inside the pipeline, holding the site.
        while !r.hold.started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        let refused = r
            .remedies
            .apply("site-1", "agent-t2", &second_plan, None)
            .await;
        gate.notify_one();
        refused
    };
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    let refused = second.unwrap_err();
    assert!(
        refused.to_string().contains("one run at a time"),
        "{refused}"
    );
    assert_eq!(r.worlds.live_health(), Some(IndexHealth::Fresh));
}

#[tokio::test]
async fn undo_inside_the_window_restores_the_site_and_a_late_undo_is_refused() {
    let r = rig(NORMAL);
    let before = r.worlds.live_state();
    r.apply().await.unwrap();
    let fix = r.fix_id();
    let undone = r.fix.undo(&fix, "operator asked").await.unwrap();
    assert_eq!(undone.state, FixState::RolledBack);
    assert_eq!(
        r.worlds.live_state(),
        before,
        "undo restores the pre-state exactly"
    );

    // A second fix, undone after the window: refused, and the change stands.
    let rig2 = rig(NORMAL);
    rig2.apply().await.unwrap();
    let fix2 = rig2.fix_id();
    rig2.clock.advance(Duration::from_secs(86_400 + 1));
    let late = rig2.fix.undo(&fix2, "too late").await.unwrap_err();
    assert_eq!(late.code(), "fix_pipeline.undo_expired");
    assert_eq!(rig2.worlds.live_health(), Some(IndexHealth::Fresh));
}

#[tokio::test]
async fn maintenance_closes_expired_windows_and_recovery_fails_closed() {
    let r = rig(NORMAL);
    r.apply().await.unwrap();
    // Nothing to do yet.
    assert_eq!(r.fix.maintain().await.unwrap().closed, 0);
    r.clock.advance(Duration::from_secs(86_400 + 1));
    let report = r.fix.maintain().await.unwrap();
    assert_eq!(report.closed, 1);
    assert!(r.trail().contains(&"UndoWindowClosed".to_owned()));
    assert_eq!(
        r.fix.maintain().await.unwrap().closed,
        0,
        "the window closes once"
    );

    // A fix the journal shows planned but never finished (the process ended): recovery cannot
    // rehearse it without a run, so it ends failed with evidence rather than being guessed at.
    let orphan = FixId::new("fix-orphan-1").unwrap();
    let plan = FixPlan::seal(
        FixSource::Remedy {
            remedy: index_remedy(),
            plan_hash: "abc".to_owned(),
        },
        Tier::T2,
        vec![FixStep::new(
            0,
            "remedy-0",
            "index:retrieval",
            "index_rebuild",
            "rebuild",
        )],
        Risk::Low,
        true,
        Vec::new(),
    )
    .unwrap();
    r.journal
        .append(
            &orphan,
            1_795_262_400,
            JournalRecord::Proposed {
                site: "site-1".to_owned(),
                agent: AgentRef::new("agent-t2", Tier::T2, false),
                epoch: DeploymentEpoch::new(1),
            },
        )
        .unwrap();
    r.journal
        .append(
            &orphan,
            1_795_262_400,
            JournalRecord::Planned {
                plan: Box::new(plan),
                path: PlanPath::Rehearse,
            },
        )
        .unwrap();
    let report = r.fix.maintain().await.unwrap();
    assert_eq!((report.recovered, report.failed), (1, 1));
    let outcome = r.fix.status(&orphan).await.unwrap();
    assert_eq!(outcome.state, FixState::Failed);
}
