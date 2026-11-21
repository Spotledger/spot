//! Shared test harness: a whole fake world and an engine over it.
//!
//! Everything is in memory and deterministic. The same `Arc`s are handed to a "restart" (a fresh
//! engine over the same journal, gate, hold, snapshot and runner), which is what the fault
//! injection tests need.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::sync::Arc;

use spot_migration_apply::testing::{
    FakeRunner, FakeSite, FakeSnapshotPort, FakeVerify, site_id, ticket_for,
};
use spot_migration_apply::{
    ApplyEngine, ApplyRequest, Faults, InMemoryAdmissionGate, InMemoryJournal, KillAt,
    RecordingHold, RecordingProgress, SnapshotId, SnapshotPort,
};
use spot_migration_plan::ReleasePlan;

/// The fake world and the engine's collaborators.
pub struct Harness {
    /// The site the runner writes and the snapshot restores.
    pub site: Arc<FakeSite>,
    /// The write-ahead journal.
    pub journal: Arc<InMemoryJournal>,
    /// The maintenance gate.
    pub gate: Arc<InMemoryAdmissionGate>,
    /// The side-effect hold.
    pub hold: Arc<RecordingHold>,
    /// The snapshot port.
    pub snapshot: Arc<FakeSnapshotPort>,
    /// The step runner.
    pub runner: Arc<FakeRunner>,
    /// The verifier.
    pub verify: Arc<FakeVerify>,
    /// The progress transcript.
    pub progress: Arc<RecordingProgress>,
}

impl Harness {
    /// A harness whose runner is `configure(FakeRunner::new(site))` and whose verifier passes.
    pub fn new(configure: impl FnOnce(FakeRunner) -> FakeRunner) -> Self {
        let site = Arc::new(FakeSite::new());
        let snapshot = Arc::new(FakeSnapshotPort::new(Arc::clone(&site)));
        let runner = Arc::new(configure(FakeRunner::new(Arc::clone(&site))));
        Self {
            site,
            journal: Arc::new(InMemoryJournal::new()),
            gate: Arc::new(InMemoryAdmissionGate::new()),
            hold: Arc::new(RecordingHold::new()),
            snapshot,
            runner,
            verify: Arc::new(FakeVerify::passing()),
            progress: Arc::new(RecordingProgress::new()),
        }
    }

    /// An engine over the harness's collaborators, with no faults.
    pub fn engine(&self) -> ApplyEngine {
        self.engine_with(Arc::new(spot_migration_apply::NoFaults))
    }

    /// An engine over the harness's collaborators, with the given fault switch.
    pub fn engine_with(&self, faults: Arc<dyn Faults>) -> ApplyEngine {
        ApplyEngine::new(
            site_id(),
            self.journal.clone(),
            self.gate.clone(),
            self.hold.clone(),
            self.snapshot.clone(),
            self.runner.clone(),
            self.verify.clone(),
        )
        .with_progress(self.progress.clone())
        .with_faults(faults)
    }

    /// An engine over the harness's collaborators but with a different verifier.
    pub fn engine_with_verify(
        &self,
        verify: Arc<dyn spot_migration_apply::VerifyPort>,
    ) -> ApplyEngine {
        ApplyEngine::new(
            site_id(),
            self.journal.clone(),
            self.gate.clone(),
            self.hold.clone(),
            self.snapshot.clone(),
            self.runner.clone(),
            verify,
        )
        .with_progress(self.progress.clone())
    }

    /// An engine that kills at `point`.
    pub fn engine_killing_at(&self, point: spot_migration_apply::KillPoint) -> ApplyEngine {
        self.engine_with(Arc::new(KillAt::new(point)))
    }

    /// Takes a snapshot of the current site.
    pub async fn take(&self) -> SnapshotId {
        self.snapshot.take("test", 0).await.unwrap()
    }

    /// A request for `plan` with a ticket and `snapshot`.
    pub fn request(&self, plan: &ReleasePlan, snapshot: SnapshotId, now: i64) -> ApplyRequest {
        ApplyRequest::new(
            plan.clone(),
            Some(ticket_for(plan, now).unwrap()),
            Some(snapshot),
            now,
        )
    }
}

/// The step ids a plan's steps will apply, in order.
pub fn step_ids(plan: &ReleasePlan) -> Vec<String> {
    plan.steps().iter().map(|step| step.id.clone()).collect()
}

/// Whether, after a crash, the journal says the gate must still be closed.
///
/// The run engages the gate when the hold begins or a step or verification starts; the gate may
/// only be open once the run reached `Verified` or `Restored` (the two paths that open it). So a
/// crash that engaged the gate but did not reach either must leave it closed.
pub fn gate_must_be_closed(harness: &Harness) -> bool {
    let records = harness.journal.written();
    let engaged = records.iter().any(|entry| {
        matches!(
            entry.record,
            spot_migration_apply::JournalRecord::HoldBegun
                | spot_migration_apply::JournalRecord::StepStarted { .. }
                | spot_migration_apply::JournalRecord::VerifyStarted
        )
    });
    let open_allowed = records.iter().any(|entry| {
        matches!(
            entry.record,
            spot_migration_apply::JournalRecord::Verified
                | spot_migration_apply::JournalRecord::Restored { .. }
                | spot_migration_apply::JournalRecord::GateOpened { .. }
        )
    });
    engaged && !open_allowed
}
