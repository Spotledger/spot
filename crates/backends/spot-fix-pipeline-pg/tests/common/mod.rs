//! A scratch database, a bridge on it, a journal over the bridge, and a rig that drives the fix
//! engine over any `FixJournal`. Nothing reads a clock.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use spot_agent_tiers::Tier;
use spot_fix_pipeline::testing::{
    FakeApproval, FakeExecutor, FakePreImages, FakeRehearsal, FakeSnapshots, FakeVerifier,
    FakeWorld,
};
use spot_fix_pipeline::{
    Faults, FixContext, FixEngine, FixId, FixJournal, FixOutcome, FixPlan, FixPolicy, FixPorts,
    FixSource, FixStep, MemoryFixAudit, NoFaults, Result, Risk, StructuralKind,
};
use spot_fix_pipeline_pg::PgFixJournal;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_remedies::RemedyId;

pub const REMEDY: &str = "job.retry_failed";

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// A scratch database, or `None` (and a printed `SKIPPED`) without `SPOT_TEST_PG_URL`.
pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

pub fn backend(fx: &Fixture, pool: usize) -> PgBackend {
    let mut config = fx.db.config();
    config.pool_max_size = pool;
    PgBackend::connect_unchecked(config).expect("backend")
}

pub fn bridge_config(name: &str, max_in_flight: usize) -> BridgeConfig {
    BridgeConfig {
        name: name.to_owned(),
        workers: 2,
        max_in_flight,
        queue_capacity: 64,
        default_deadline: Duration::from_secs(5),
        max_deadline: Duration::from_secs(20),
    }
}

/// A prepared journal on a fresh bridge (a "node") over the fixture's database.
pub fn node(fx: &Fixture, name: &str) -> PgFixJournal {
    let backend = backend(fx, 8);
    let bridge = Arc::new(Bridge::for_backend(bridge_config(name, 6), &backend).expect("bridge"));
    let journal = PgFixJournal::new(bridge, &backend.pool()).expect("journal");
    journal.ensure_schema().expect("schema");
    journal
}

/// What a scenario leaves behind, comparable across journals.
#[derive(Debug, PartialEq, Eq)]
pub struct Observed {
    /// How the first run ended: `killed:<point>` or `finished:<state>`.
    pub first: String,
    /// The state after recovery, if the run was killed.
    pub recovered: Option<String>,
    /// The world after everything.
    pub world: BTreeMap<String, i128>,
    /// Every journal entry of the fix, without its sequence number.
    pub entries: Vec<(i64, spot_fix_pipeline::JournalRecord)>,
}

/// Which plan a rig runs.
#[derive(Clone, Copy)]
pub enum Kind {
    /// T1, direct apply, everything passes.
    Small,
    /// T1, the symptom re-check fails, so the run rolls back.
    SymptomFails,
    /// T3 structural, snapshot mechanism.
    Structural,
}

pub struct Rig {
    pub world: FakeWorld,
    plan: FixPlan,
    ctx: FixContext,
    policy: FixPolicy,
    audit: MemoryFixAudit,
    pre: FakePreImages,
    snaps: FakeSnapshots,
    exec: FakeExecutor,
    verifier: FakeVerifier,
    rehearsal: FakeRehearsal,
    approval: FakeApproval,
}

impl Rig {
    pub fn new(kind: Kind) -> Self {
        let (targets, tier): (&[(&str, i128)], Tier) = match kind {
            Kind::Small | Kind::SymptomFails => (&[("job:1", 10), ("job:2", 20)], Tier::T1),
            Kind::Structural => (&[("meta:party", 1), ("meta:item", 2)], Tier::T3),
        };
        let world = FakeWorld::with(targets);
        let verifier = match kind {
            Kind::SymptomFails => FakeVerifier::symptom_fails(&world),
            _ => FakeVerifier::passing(&world),
        };
        let steps = targets
            .iter()
            .enumerate()
            .map(|(i, (target, _))| {
                FixStep::new(i, &format!("s{i}"), target, "job.retry", "retry the job")
            })
            .collect();
        let source = if tier == Tier::T3 {
            FixSource::Structural {
                structural: StructuralKind::Migration,
                plan_hash: "structural-plan".to_owned(),
            }
        } else {
            FixSource::Remedy {
                remedy: RemedyId::new(REMEDY),
                plan_hash: "remedy-plan".to_owned(),
            }
        };
        let plan = FixPlan::seal(source, tier, steps, Risk::Low, true, Vec::new()).expect("plan");
        let policy = if tier == Tier::T3 {
            FixPolicy::default()
        } else {
            FixPolicy::default().with_rehearsal_optional(REMEDY)
        };
        Self {
            pre: FakePreImages::new(&world),
            snaps: FakeSnapshots::new(&world),
            exec: FakeExecutor::new(&world, 1),
            verifier,
            rehearsal: FakeRehearsal::passing(&world),
            approval: FakeApproval::approving("alice"),
            ctx: spot_fix_pipeline::testing::context("site-1", tier, false, 1_000),
            world,
            plan,
            policy,
            audit: MemoryFixAudit::new(),
        }
    }

    fn engine<'a>(
        &'a mut self,
        journal: &'a dyn FixJournal,
        faults: &'a dyn Faults,
    ) -> (FixEngine<'a>, FixPlan, FixContext) {
        let plan = self.plan.clone();
        let ctx = self.ctx.clone();
        let ports = FixPorts {
            pre_images: &mut self.pre,
            snapshots: &mut self.snaps,
            executor: &mut self.exec,
            verifier: &mut self.verifier,
            rehearsal: &mut self.rehearsal,
            approval: &mut self.approval,
        };
        let engine = FixEngine::new(self.policy.clone(), journal, &mut self.audit, ports, faults);
        (engine, plan, ctx)
    }

    pub fn run(
        &mut self,
        journal: &dyn FixJournal,
        fix: &FixId,
        faults: &dyn Faults,
    ) -> Result<FixOutcome> {
        let (mut engine, plan, ctx) = self.engine(journal, faults);
        engine.run(fix, &plan, &ctx)
    }

    pub fn recover(&mut self, journal: &dyn FixJournal, fix: &FixId) -> Result<FixOutcome> {
        let (mut engine, _, ctx) = self.engine(journal, &NoFaults);
        engine.recover(fix, &ctx)
    }

    /// Runs with `faults`, recovers if killed, and reports everything comparable.
    pub fn scenario(
        &mut self,
        journal: &dyn FixJournal,
        fix: &FixId,
        faults: &dyn Faults,
    ) -> Observed {
        let (first, recovered) = match self.run(journal, fix, faults) {
            Ok(outcome) => (format!("finished:{}", outcome.state), None),
            Err(spot_fix_pipeline::FixError::Killed { at }) => {
                let outcome = self.recover(journal, fix).expect("recover");
                (format!("killed:{at}"), Some(outcome.state.to_string()))
            }
            Err(other) => panic!("unexpected error: {other}"),
        };
        let entries = journal
            .entries(fix)
            .expect("entries")
            .into_iter()
            .map(|e| (e.at, e.record))
            .collect();
        Observed {
            first,
            recovered,
            world: self.world.snapshot().unwrap(),
            entries,
        }
    }
}
