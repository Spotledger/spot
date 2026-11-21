//! The self-test harness: `remedy_test::run` proves one remedy end to end.
//!
//! The harness builds the scenario's fixture on the in-memory fake, checks the precondition,
//! dry-runs, applies, asserts the after-apply invariant, then undoes (or expects the refusal for
//! an irreversible remedy) and asserts the after-undo invariant. The registry test enumerates
//! every remedy and runs its scenario, so a remedy without a passing scenario fails CI.

use std::fmt;

use spot_agent_tiers::{AgentIdentity, AgentRegistry, Domain, Tier};

use crate::audit::AuditKind;
use crate::ctx::{Gate, RemedySettings};
use crate::error::RemedyError;
use crate::fixture::FakeWorld;
use crate::model::RemedyId;
use crate::registry::Registry;
use crate::scenario::{TestSpec, UndoExpectation};

/// What the undo step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoOutcome {
    /// The undo ran and restored the world.
    Restored,
    /// The remedy is irreversible and the undo was refused, as expected.
    Refused,
}

/// What a passing run produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The scenario's name.
    pub scenario: String,
    /// The remedy.
    pub remedy: RemedyId,
    /// How many plan steps the dry-run produced.
    pub plan_steps: usize,
    /// How many changes the apply made.
    pub changes: usize,
    /// What undo did.
    pub undo: UndoOutcome,
    /// The audit kinds recorded, in order.
    pub audit: Vec<AuditKind>,
}

/// A step that failed: the scenario, which step, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The scenario's name.
    pub scenario: String,
    /// The step (`precondition`, `dry-run`, `apply`, `invariant-after-apply`, `undo`,
    /// `invariant-after-undo`).
    pub step: &'static str,
    /// What went wrong.
    pub detail: String,
}

impl Failure {
    /// A failure.
    #[must_use]
    pub fn new(scenario: &str, step: &'static str, detail: &str) -> Self {
        Self {
            scenario: scenario.to_owned(),
            step,
            detail: detail.to_owned(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "remedy self-test `{}` failed at {}: {}",
            self.scenario, self.step, self.detail
        )
    }
}

impl std::error::Error for Failure {}

/// The instant every scenario runs at (deterministic; time is an argument).
pub const SCENARIO_NOW: i64 = 1_700_000_000;

/// A registered-deep support agent, so a T3 remedy may run unattended.
fn deep_agent() -> Result<AgentIdentity, Failure> {
    let mut registry = AgentRegistry::new();
    let agent = AgentIdentity::new("agent-harness", "support-org", Domain::Support, Tier::T3);
    registry
        .enroll(agent)
        .map_err(|error| Failure::new("harness", "agent", &error.to_string()))?;
    registry
        .register_deep("agent-harness", "support-lead", "harness-evidence", 0)
        .map_err(|error| Failure::new("harness", "agent", &error.to_string()))?;
    registry
        .get("agent-harness")
        .cloned()
        .ok_or_else(|| Failure::new("harness", "agent", "the harness agent vanished"))
}

/// Runs one remedy's scenario against the registry.
///
/// # Errors
///
/// A [`Failure`] naming the step that did not hold.
pub fn run(registry: &Registry, id: &RemedyId, spec: &TestSpec) -> Result<Report, Failure> {
    let scenario = &spec.scenario;
    let agent = deep_agent()?;
    let settings: RemedySettings = registry.settings().clone();
    let gate = Gate::change();
    let mut world = FakeWorld::from_spec(&scenario.world);

    // Precondition: it must be ready before anything runs.
    {
        let remedy = registry.get(id).ok_or_else(|| {
            Failure::new(
                &scenario.name,
                "precondition",
                "the remedy is not registered",
            )
        })?;
        let mut ctx = world.ctx("agent-harness", SCENARIO_NOW, &settings);
        let check = remedy.precondition(&mut ctx, &scenario.params);
        if !check.is_ready() {
            return Err(Failure::new(
                &scenario.name,
                "precondition",
                check.summary(),
            ));
        }
    }

    // Dry-run.
    let plan = {
        let mut ctx = world.ctx("agent-harness", SCENARIO_NOW, &settings);
        registry
            .dry_run(&agent, id, &scenario.params, &gate, &mut ctx)
            .map_err(|error| Failure::new(&scenario.name, "dry-run", &error.sentence()))?
    };

    // Apply the plan dry-run produced.
    let applied = {
        let mut ctx = world.ctx("agent-harness", SCENARIO_NOW, &settings);
        registry
            .apply(&agent, id, &plan, &gate, &mut ctx)
            .map_err(|error| Failure::new(&scenario.name, "apply", &error.sentence()))?
    };

    // The fault is repaired.
    (scenario.expect_apply)(&world)
        .map_err(|detail| Failure::new(&scenario.name, "invariant-after-apply", &detail))?;

    // Undo, or expect the refusal.
    let undo = match scenario.undo {
        UndoExpectation::Restored => {
            let mut ctx = world.ctx("agent-harness", SCENARIO_NOW, &settings);
            registry
                .undo(&agent, id, &applied.undo_token, &gate, &mut ctx)
                .map_err(|error| Failure::new(&scenario.name, "undo", &error.sentence()))?;
            UndoOutcome::Restored
        }
        UndoExpectation::Refused => {
            let mut ctx = world.ctx("agent-harness", SCENARIO_NOW, &settings);
            match registry.undo(&agent, id, &applied.undo_token, &gate, &mut ctx) {
                Err(RemedyError::Irreversible { .. }) => UndoOutcome::Refused,
                Err(other) => {
                    return Err(Failure::new(
                        &scenario.name,
                        "undo",
                        &format!("expected an irreversible refusal, got {}", other.sentence()),
                    ));
                }
                Ok(_) => {
                    return Err(Failure::new(
                        &scenario.name,
                        "undo",
                        "expected an irreversible refusal, got a successful undo",
                    ));
                }
            }
        }
    };

    // The world is restored (or unchanged, for a refused undo).
    (scenario.expect_undo)(&world)
        .map_err(|detail| Failure::new(&scenario.name, "invariant-after-undo", &detail))?;

    Ok(Report {
        scenario: scenario.name.clone(),
        remedy: id.clone(),
        plan_steps: plan.steps.len(),
        changes: applied.changes.len(),
        undo,
        audit: world.audit.kinds(),
    })
}
