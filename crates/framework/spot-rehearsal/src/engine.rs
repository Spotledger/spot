//! The rehearsal engine: plan the clone, measure, apply, measure again, check and report.

use std::sync::Arc;

use spot_core::SiteId;
use spot_migration_approval::{ApplyTicket, PlanDigest};
use spot_migration_plan::ReleasePlan;
use spot_snapshot::{ProgressSink, SnapshotProvider};

use crate::applier::{Applier, ApplyOutcome};
use crate::clone::{CloneRequest, RehearsalClone, SilentProgress, obtain_clone};
use crate::declaration::{NoDeclaration, ReleaseDeclaration};
use crate::error::{Error, Result};
use crate::fingerprint::{Fingerprint, FingerprintDiff};
use crate::invariants::{InvariantContext, InvariantReport, InvariantSet};
use crate::replay::{ReplayPolicy, decide_replay};
use crate::report::{ClassifiedDiff, CloneRecord, RehearsalReport};
use crate::spec::{FingerprintSource, FingerprintSpec};

/// The rehearsal engine. It owns the provider, the applier, the fingerprint source, the spec, the
/// clone request and the invariant sets; [`run`](Rehearsal::run) puts them together.
pub struct Rehearsal {
    provider: Arc<dyn SnapshotProvider>,
    applier: Arc<dyn Applier>,
    fingerprint: Arc<dyn FingerprintSource>,
    spec: FingerprintSpec,
    request: CloneRequest,
    invariants: Vec<Arc<dyn InvariantSet>>,
    declaration: Arc<dyn ReleaseDeclaration>,
    policy: ReplayPolicy,
    progress: Arc<dyn ProgressSink>,
}

impl Rehearsal {
    /// A rehearsal with no invariants, no release declaration, the `off` replay policy and a
    /// silent progress sink.
    pub fn new(
        provider: Arc<dyn SnapshotProvider>,
        applier: Arc<dyn Applier>,
        fingerprint: Arc<dyn FingerprintSource>,
        spec: FingerprintSpec,
        request: CloneRequest,
    ) -> Rehearsal {
        Rehearsal {
            provider,
            applier,
            fingerprint,
            spec,
            request,
            invariants: Vec::new(),
            declaration: Arc::new(NoDeclaration),
            policy: ReplayPolicy::Off,
            progress: Arc::new(SilentProgress),
        }
    }

    /// Registers the invariant sets to run after the plan is applied.
    #[must_use]
    pub fn with_invariants(mut self, sets: Vec<Arc<dyn InvariantSet>>) -> Rehearsal {
        self.invariants = sets;
        self
    }

    /// Sets the release declaration (WP-MIG-02's release metadata).
    #[must_use]
    pub fn with_declaration(mut self, declaration: Arc<dyn ReleaseDeclaration>) -> Rehearsal {
        self.declaration = declaration;
        self
    }

    /// Sets the per-site replay policy.
    #[must_use]
    pub fn with_replay_policy(mut self, policy: ReplayPolicy) -> Rehearsal {
        self.policy = policy;
        self
    }

    /// Sets the progress sink the take and the restore report through.
    #[must_use]
    pub fn with_progress(mut self, progress: Arc<dyn ProgressSink>) -> Rehearsal {
        self.progress = progress;
        self
    }

    /// Rehearses `plan` for `site` under `ticket`, the proof `authorise_apply` issued.
    ///
    /// # Errors
    /// [`Error::TicketMismatch`] when the ticket does not authorise exactly this plan (checked
    /// before any clone is made); then the snapshot, apply and fingerprint errors of the steps.
    /// The clone is destroyed on every exit path.
    pub async fn run(
        &self,
        plan: &ReleasePlan,
        ticket: &ApplyTicket,
        site: &SiteId,
    ) -> Result<RehearsalReport> {
        if !ticket.matches(plan) {
            return Err(Error::TicketMismatch {
                ticket: ticket.digest().as_hex().to_owned(),
                plan: PlanDigest::of(plan).as_hex().to_owned(),
            });
        }
        self.spec.validate()?;
        let replay = decide_replay(self.policy, plan, self.declaration.as_ref());

        let mut clone = obtain_clone(
            self.provider.as_ref(),
            &self.request,
            site,
            self.progress.clone(),
        )
        .await?;
        let measured = self.measure_and_apply(plan, &clone).await;
        // Closed here on success and failure alike, so a failed drop is awaited, not left to
        // `Drop`; on success it goes into the report.
        let cleanup = clone.close_async().await;
        let (pre, apply, post, invariants, diffs) = measured?;
        let clone_record = CloneRecord {
            site: site.to_string(),
            label: clone.target().label().to_owned(),
            snapshot_id: clone.snapshot_id().to_owned(),
            cleanup,
        };
        let outcome = RehearsalReport::outcome_of(&diffs, &invariants);
        Ok(RehearsalReport {
            ticket: ticket.digest().as_hex().to_owned(),
            site: site.to_string(),
            plan_digest: plan.plan_digest.clone(),
            clone: clone_record,
            pre_digest: pre.digest(),
            post_digest: post.digest(),
            invariants,
            diffs,
            replay,
            apply,
            outcome,
        })
    }

    /// Measures, applies, measures again and checks, all against the clone.
    async fn measure_and_apply(
        &self,
        plan: &ReleasePlan,
        clone: &RehearsalClone,
    ) -> Result<Measured> {
        let pre = self
            .spec
            .measure(self.fingerprint.as_ref(), clone.target())
            .await?;
        let apply = self.applier.apply(plan, clone.target()).await?;
        let post = self
            .spec
            .measure(self.fingerprint.as_ref(), clone.target())
            .await?;
        let context = InvariantContext {
            clone: clone.target(),
            plan,
            fingerprint: &post,
        };
        let invariants = InvariantReport::run(&self.invariants, &context).await;
        let diffs = classify(&pre.diff(&post), plan, self.declaration.as_ref());
        Ok((pre, apply, post, invariants, diffs))
    }
}

/// What the steps against the clone produce.
type Measured = (
    Fingerprint,
    ApplyOutcome,
    Fingerprint,
    InvariantReport,
    Vec<ClassifiedDiff>,
);

/// Classifies every difference: a difference in a section a plan change declares is expected,
/// every other difference is unexplained.
fn classify(
    diffs: &[FingerprintDiff],
    plan: &ReleasePlan,
    declaration: &dyn ReleaseDeclaration,
) -> Vec<ClassifiedDiff> {
    diffs
        .iter()
        .map(|diff| {
            let change = plan.changes.iter().find(|change| {
                declaration
                    .expected_sections(change)
                    .iter()
                    .any(|section| section == &diff.section)
            });
            match change {
                Some(change) => ClassifiedDiff::expected(diff, change.id.clone()),
                None => ClassifiedDiff::unexplained(diff),
            }
        })
        .collect()
}
