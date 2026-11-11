//! Seeded-corruption proofs at engine level (WP-TST-07.T2).
//!
//! A suite that passes on a broken system proves nothing. Each function here breaks exactly one
//! of the engine's promises and the tests show that **exactly** the named invariant fails. Most
//! corruptions are applied to the run's oracle (the effects the driver observed) — which is
//! precisely what a weakened engine would make the store look like — because the in-memory fake
//! keeps its state private; [`enqueue_orphan_event`] additionally corrupts the store itself,
//! through the contract, to prove `events-per_operation` catches an event that was published for
//! a transaction that never committed.

use std::sync::Arc;

use spot_app_api::DocState;
use spot_app_api::prelude::RecordId;
use spot_document::engine::Engine;
use spot_events_contract::lifecycle::Operation;
use spot_l7_invariants::invariant::Violation;
use spot_l7_invariants::schedule::Profile;
use spot_storage_contract::{
    Isolation, Name, NodeRef, Outbox, OutboxEvent, StorageBackend, StorageResult, Transaction,
    TxOptions,
};

use crate::driver::{DriverConfig, EngineRun, ExpectedEvent, Op};
use crate::jobs::JobRun;
use crate::naming::NamingRun;

/// What a corruption proof found.
#[derive(Clone, Debug)]
pub struct Proof {
    /// The corruption's name.
    pub corruption: &'static str,
    /// Invariants that failed before the corruption (must be none).
    pub failed_before: Vec<String>,
    /// Invariants that failed after it.
    pub failed_after: Vec<String>,
    /// The violations of the expected invariants.
    pub violations: Vec<Violation>,
    /// The seed, to replay the schedule.
    pub seed: u64,
}

impl Proof {
    /// Whether the proof holds: clean before, and exactly `expected` failing after, each with a
    /// violation naming it.
    pub fn holds(&self, expected: &[&str]) -> bool {
        if !self.failed_before.is_empty() {
            return false;
        }
        let mut want: Vec<&str> = expected.to_vec();
        want.sort_unstable();
        let mut got: Vec<&str> = self.failed_after.iter().map(String::as_str).collect();
        got.sort_unstable();
        if want != got {
            return false;
        }
        expected
            .iter()
            .all(|id| self.violations.iter().any(|v| v.invariant == *id))
    }
}

// ---------------------------------------------------------------------------------------------
// Engine oracle corruptions
// ---------------------------------------------------------------------------------------------

/// Rewinds a live document's state in the oracle: a store that rewound a posted Time Entry, or
/// gave a plain document a lifecycle state it never had.
pub fn state_rewound(run: &EngineRun) -> EngineRun {
    let mut out = run.clone();
    if let Some(doc) = out.docs.values_mut().find(|d| d.live) {
        doc.state = Some(match doc.state {
            Some(DocState::Draft) => DocState::Posted,
            Some(DocState::Posted) => DocState::Draft,
            Some(DocState::Cancelled) => DocState::Posted,
            None => DocState::Posted,
        });
    }
    out
}

/// Removes the expected event of one committed write (a submit, when there is one): an event the
/// engine should have written in the transaction but did not.
pub fn missing_event(run: &EngineRun) -> EngineRun {
    let mut out = run.clone();
    let index = out
        .events
        .iter()
        .position(|e| e.op == Op::PostTimeEntry)
        .or(if out.events.is_empty() { None } else { Some(0) });
    if let Some(i) = index {
        out.events.remove(i);
    }
    out
}

/// Adds an expected event for a subject the store does not hold: an event for a transaction
/// that did not commit.
pub fn orphan_event(run: &EngineRun) -> EngineRun {
    let mut out = run.clone();
    out.events.push(ExpectedEvent {
        op: Op::PostTimeEntry,
        topic: Operation::Post.topic_name(),
        entity: crate::world::TIME_ENTRY.to_owned(),
        id: RecordId::new_v7(),
        version: 1,
        actor: "manager".to_owned(),
    });
    out
}

/// Plants a secret that the lifecycle events actually carry (a document's name), as a leaked
/// protected value would.
pub fn secret_in_event(run: &EngineRun) -> EngineRun {
    let mut out = run.clone();
    if let Some(name) = out
        .docs
        .values()
        .find(|d| d.live && !d.name.is_empty())
        .map(|d| d.name.clone())
    {
        out.secrets.push(name);
    }
    out
}

/// Records a denial for a create whose document exists: a weakened engine that skipped the
/// permission check for that DocType.
pub fn denied_create_took_effect(run: &EngineRun) -> EngineRun {
    let mut out = run.clone();
    if let Some((entity, id)) = out
        .docs
        .iter()
        .find(|((e, _), d)| d.live && e != crate::world::LABEL && e != crate::world::USER)
        .map(|((e, id), _)| (e.clone(), *id))
    {
        let op = match entity.as_str() {
            crate::world::PROJECT => Op::CreateProject,
            crate::world::TIME_ENTRY => Op::CreateTimeEntry,
            _ => Op::CreateTask,
        };
        out.denials.push(crate::driver::DeniedOp {
            op,
            entity,
            id,
            actor: "viewer".to_owned(),
            code: "permission.denied".to_owned(),
        });
    }
    out
}

/// Records a version the oracle never produced for a document that a denied write touched.
pub fn denied_write_left_version(run: &EngineRun) -> EngineRun {
    let mut out = run.clone();
    if let Some(doc) = out
        .docs
        .values_mut()
        .find(|d| d.live && !d.versions.is_empty() && d.version > 0)
    {
        doc.versions.clear();
        out.denials.push(crate::driver::DeniedOp {
            op: Op::UpdateTask,
            entity: doc.entity.clone(),
            id: doc.id,
            actor: "viewer".to_owned(),
            code: "permission.denied".to_owned(),
        });
    }
    out
}

/// Adds a link from a committed document to a target the oracle never saw.
pub fn links_to_missing(run: &EngineRun) -> EngineRun {
    let mut out = run.clone();
    if let Some((entity, id)) = out
        .docs
        .iter()
        .find(|(_, d)| d.live)
        .map(|((e, id), _)| (e.clone(), *id))
    {
        out.links.push(crate::driver::RecordedLink {
            holder_entity: entity,
            holder_id: id,
            field: "project".to_owned(),
            target_id: RecordId::new_v7(),
        });
    }
    out
}

/// Gives two live committed documents the same name.
pub fn duplicate_name(run: &EngineRun) -> EngineRun {
    let mut out = run.clone();
    let first = out
        .docs
        .values()
        .find(|d| d.live && !d.name.is_empty())
        .map(|d| (d.entity.clone(), d.name.clone()));
    if let Some((entity, name)) = first
        && let Some(doc) = out
            .docs
            .values_mut()
            .find(|d| d.live && d.entity == entity && d.name != name)
    {
        doc.name = name;
    }
    out
}

/// Records a transition a committed document could not have taken (a non-draft document posted,
/// or a cancelled document posted again).
pub fn illegal_transition(run: &EngineRun) -> EngineRun {
    let mut out = run.clone();
    if let Some(doc) = out
        .docs
        .values_mut()
        .find(|d| d.live && d.state != Some(DocState::Draft) && !d.transitions.is_empty())
    {
        doc.transitions.push(Op::PostTimeEntry);
    } else if let Some(doc) = out
        .docs
        .values_mut()
        .find(|d| d.live && !d.transitions.is_empty())
    {
        doc.transitions.push(Op::CancelTimeEntry);
        doc.transitions.push(Op::PostTimeEntry);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Store corruption (through the contract)
// ---------------------------------------------------------------------------------------------

/// Publishes an outbox event about a subject no committed transaction wrote: a weakened engine
/// that commits its event after the transaction, or a transaction that rolled back.
pub async fn enqueue_orphan_event<B: StorageBackend>(
    engine: &Arc<Engine<B>>,
    catalog: Arc<spot_storage_contract::Catalog>,
) -> StorageResult<RecordId> {
    let options = TxOptions::new(Name::new("l7_orphan")?, Isolation::ReadCommitted);
    let mut tx = engine.backend().begin(catalog, options).await?;
    let id = RecordId::new_v7();
    tx.enqueue(&OutboxEvent {
        id,
        topic: Name::new(Operation::Post.topic_name())?,
        subject: NodeRef {
            entity: Name::new(crate::world::TIME_ENTRY)?,
            id: RecordId::new_v7(),
        },
        subject_version: Some(spot_storage_contract::Version::FIRST),
        schema_version: 1,
        payload: "{\"meta\":{},\"data\":{\"name\":\"orphan\"}}".to_owned(),
    })
    .await?;
    tx.commit().await?;
    Ok(id)
}

// ---------------------------------------------------------------------------------------------
// Naming corruptions
// ---------------------------------------------------------------------------------------------

/// Leaves a gap in a series' committed numbers (a number no document holds).
pub fn gap_in_series(run: &NamingRun) -> NamingRun {
    let mut out = run.clone();
    if let Some((series, numbers)) = out.series.iter_mut().next() {
        let top = numbers.iter().next_back().copied().unwrap_or(0);
        numbers.insert(top + 2);
        out.allocations.push(crate::naming::Allocation {
            series: series.clone(),
            number: top + 2,
            name: format!("phantom-{}", top + 2),
        });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Job corruptions
// ---------------------------------------------------------------------------------------------

/// Drops a job from the oracle: a job that was lost.
pub fn job_lost(run: &JobRun) -> JobRun {
    let mut out = run.clone();
    if !out.outcomes.is_empty() {
        out.outcomes.pop();
    }
    out
}

/// Doubles the effect of one idempotency key.
pub fn job_double_effect(run: &JobRun) -> JobRun {
    let mut out = run.clone();
    if let Some(key) = out.effects.keys().next().cloned() {
        *out.effects.entry(key).or_insert(0) += 1;
    }
    out
}

/// Lets a job run past its retry bound.
pub fn job_over_retry(run: &JobRun) -> JobRun {
    let mut out = run.clone();
    if let Some(outcome) = out.outcomes.first_mut() {
        outcome.attempts = outcome.max_attempts + 1;
    }
    out
}

/// The driver configuration a proof uses.
pub fn proof_config(seed: u64, backend: &str) -> DriverConfig {
    let mut config = DriverConfig::for_profile(Profile::Smoke, seed, backend);
    config.workers = 4;
    config.operations = 400;
    config
}
