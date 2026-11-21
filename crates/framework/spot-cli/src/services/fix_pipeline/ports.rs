//! The engine's typed ports, implemented over the site.
//!
//! The pipeline normalises a remedy's plan and drives it through `spot_fix_pipeline::FixEngine`.
//! The engine wants pre-images, a snapshot port, a step executor, a rehearsal port, a verifier, an
//! approval port and an audit port. Here they are:
//!
//! - **State comes from fingerprints.** A target's state is the remedy framework's own fingerprint
//!   of it (`RemedyCtx::fingerprint`, read through [`WorldAccess`]), so a pre-image is the digest
//!   of the target before the apply, the change record is the remedy's before/after, and a
//!   rollback is proved by the digests coming back. `PreImageStore::restore` therefore *checks*
//!   rather than writes: the inverse is the remedy's own undo, which the executor runs first; a
//!   target that does not read back as it was is a failed rollback, never a claimed one.
//! - **A remedy applies as one unit.** Its steps are one plan, so the first `run_step` applies the
//!   remedy live (after engaging the effect hold); later steps read their share of the recorded
//!   changes. `undo_step` runs the remedy's undo once.
//! - **A rehearsal is the remedy applied on the rehearsal snapshot**, never on the live site.
//! - **Verification** re-reads every changed target (the state must still be what the apply left)
//!   and runs the site's [`FixChecks`] (invariants and the symptom re-check); a failure rolls the
//!   change back.
//! - **No approval inbox here**: a T3 plan a deep agent may not self-approve is refused.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use spot_fix_pipeline::{
    ApprovalPort, ChangeRecord, FixAudit, FixAuditEvent, FixError, FixExecutor, FixId, FixPlan,
    FixStep, FixVerifier, PreImage, PreImageStore, RehearsalEvidence, RehearsalPort, Result,
    SnapshotPort, SnapshotRef, StepChange, VerifyReport,
};
use spot_rehearsal::{Dimension, Fingerprint, FingerprintRow, FingerprintSection, InvariantReport};
use spot_remedies::{Applied, RemedySettings};

use super::link::{Link, Req};
use crate::services::remedies::world::{Collect, CtxRequest, WorldAccess, WorldKind};

/// The actor the pipeline reads state as.
const READER: &str = "fix-pipeline";

/// The site's own checks after an apply: the invariants and the symptom re-check.
pub trait FixChecks: Send + Sync {
    /// Whether the symptom the remedy was run for has cleared, and what else is wrong. Runs on the
    /// pipeline's own thread, after the apply, with the live site's changes in hand.
    fn check(&self, plan: &FixPlan, changes: &[ChangeRecord]) -> CheckOutcome;
}

/// What [`FixChecks::check`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Whether the original symptom cleared.
    pub symptom_cleared: bool,
    /// Violations, in plain language and never a value.
    pub violations: Vec<String>,
}

/// The default checks: nothing beyond the state read-back the verifier already does.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoChecks;

impl FixChecks for NoChecks {
    fn check(&self, _plan: &FixPlan, _changes: &[ChangeRecord]) -> CheckOutcome {
        CheckOutcome {
            symptom_cleared: true,
            violations: Vec::new(),
        }
    }
}

/// Reads typed targets' state fingerprints from the live world.
#[derive(Clone)]
pub struct Probe {
    world: Arc<dyn WorldAccess>,
    settings: RemedySettings,
    now: i64,
}

impl Probe {
    /// A probe over `world`.
    #[must_use]
    pub fn new(world: Arc<dyn WorldAccess>, settings: RemedySettings, now: i64) -> Self {
        Self {
            world,
            settings,
            now,
        }
    }

    /// The state fingerprint of `target`, or `<missing>` when it does not exist.
    ///
    /// # Errors
    ///
    /// A fixed sentence when the world cannot be opened.
    pub fn digest(&self, target: &str) -> std::result::Result<String, String> {
        let mut audit = Collect::default();
        let mut out: Option<String> = None;
        self.world
            .with_ctx(
                WorldKind::Live,
                CtxRequest {
                    actor: READER,
                    now: self.now,
                    settings: &self.settings,
                    audit: &mut audit,
                },
                &mut |ctx| {
                    out = Some(
                        ctx.fingerprint(target)
                            .unwrap_or_else(|| "<missing>".to_owned()),
                    );
                },
            )
            .map_err(|e| e.to_string())?;
        out.ok_or_else(|| "the world did not run the read".to_owned())
    }

    /// The fingerprint of a set of targets: one row per target, with its state digest.
    ///
    /// # Errors
    ///
    /// A fixed sentence when the world cannot be read.
    pub fn fingerprint(&self, targets: &[String]) -> std::result::Result<Fingerprint, String> {
        let mut rows = Vec::with_capacity(targets.len());
        for target in targets {
            let digest = self.digest(target)?;
            let mut measures = BTreeMap::new();
            measures.insert("present".to_owned(), 1_i128);
            rows.push(FingerprintRow::new(
                vec![
                    Dimension::new("target", target.as_str()),
                    Dimension::new("state", digest.as_str()),
                ],
                measures,
            ));
        }
        Ok(Fingerprint::new(vec![FingerprintSection::new(
            "targets",
            "Affected targets",
            vec!["target".to_owned(), "state".to_owned()],
            vec!["present".to_owned()],
            rows,
        )]))
    }
}

fn pre_image_err(detail: impl Into<String>) -> FixError {
    FixError::PreImageFailed {
        detail: detail.into(),
    }
}

/// Pre-images as fingerprints.
pub struct DigestPreImages {
    pub(super) probe: Probe,
}

impl PreImageStore for DigestPreImages {
    fn capture(&mut self, targets: &[String]) -> Result<Vec<PreImage>> {
        targets
            .iter()
            .map(|target| {
                let digest = self.probe.digest(target).map_err(pre_image_err)?;
                Ok(PreImage::new(target, &digest, json!({"state": digest})))
            })
            .collect()
    }

    fn restore(&mut self, image: &PreImage) -> Result<()> {
        // The inverse already ran (the remedy's undo); this proves the target reads back as it was.
        let current = self.probe.digest(&image.target).map_err(pre_image_err)?;
        if current == image.digest {
            Ok(())
        } else {
            Err(pre_image_err(format!(
                "`{}` does not read back as it was before the fix",
                image.target
            )))
        }
    }

    fn digest(&mut self, target: &str) -> Result<String> {
        self.probe.digest(target).map_err(pre_image_err)
    }
}

/// Snapshots through the link.
pub struct LinkSnapshots {
    pub(super) link: Link,
}

impl SnapshotPort for LinkSnapshots {
    fn take(&mut self, fix: &FixId) -> Result<SnapshotRef> {
        self.link
            .ask(|reply| Req::SnapshotTake(fix.clone(), reply))
            .map_err(|detail| FixError::SnapshotFailed { detail })?
            .map_err(|detail| FixError::SnapshotFailed { detail })
    }

    fn restore(&mut self, snapshot: &SnapshotRef) -> Result<()> {
        self.link
            .ask(|reply| Req::SnapshotRestore(snapshot.clone(), reply))
            .map_err(|detail| FixError::SnapshotFailed { detail })?
            .map_err(|detail| FixError::SnapshotFailed { detail })
    }
}

/// Runs the remedy as one unit and reads each step's share of what it changed.
pub struct RemedyExecutor {
    pub(super) link: Link,
    /// What the live apply produced (also what the host answers its caller with).
    pub applied: Option<Applied>,
    /// `true` when this executor can run the remedy (a run), `false` after a restart, when the
    /// framework's in-memory undo log is gone and nothing can be reversed.
    pub(super) can_apply: bool,
    pub(super) reverted: bool,
}

impl RemedyExecutor {
    /// An executor over `link`.
    #[must_use]
    pub fn new(link: Link, can_apply: bool) -> Self {
        Self {
            link,
            applied: None,
            can_apply,
            reverted: false,
        }
    }
}

impl FixExecutor for RemedyExecutor {
    fn run_step(&mut self, step: &FixStep, _idempotency_key: &str) -> Result<StepChange> {
        let failed = |detail: String| FixError::StepFailed {
            index: step.index,
            id: step.id.clone(),
            detail,
        };
        if self.applied.is_none() {
            if !self.can_apply {
                return Err(failed(
                    "this process cannot apply the remedy (it was not started by a run)".to_owned(),
                ));
            }
            // The hold goes on before the first live change and stays until the fix is verified.
            self.link
                .ask(|reply| Req::Hold(format!("fix step {}", step.id), reply))
                .map_err(&failed)?
                .map_err(&failed)?;
            let applied = self
                .link
                .ask(Req::ApplyLive)
                .map_err(&failed)?
                .map_err(&failed)?;
            self.applied = Some(applied);
        }
        let applied = self
            .applied
            .as_ref()
            .ok_or_else(|| failed("the remedy left no record".to_owned()))?;
        Ok(
            match applied.changes.iter().find(|c| c.target == step.target) {
                Some(change) => StepChange {
                    before: change.before.clone(),
                    after: change.after.clone(),
                },
                // A step the remedy recorded no change for left its target as it was.
                None => StepChange {
                    before: "<unchanged>".to_owned(),
                    after: "<unchanged>".to_owned(),
                },
            },
        )
    }

    fn undo_step(&mut self, step: &FixStep, _change: &ChangeRecord) -> Result<()> {
        if self.reverted {
            return Ok(());
        }
        let failed = |detail: String| FixError::StepFailed {
            index: step.index,
            id: step.id.clone(),
            detail,
        };
        let Some(applied) = &self.applied else {
            return Err(FixError::RollbackFailed {
                detail: "the undo token is not held: the process that applied the fix has \
                         ended and the remedy undo log did not survive"
                    .to_owned(),
            });
        };
        let token = applied.undo_token.clone();
        self.link
            .ask(|reply| Req::Undo(token, reply))
            .map_err(&failed)?
            .map_err(&failed)?;
        self.reverted = true;
        Ok(())
    }
}

/// Rehearsal: the remedy applied on the rehearsal snapshot.
pub struct SnapshotRehearsal {
    pub(super) link: Link,
}

impl RehearsalPort for SnapshotRehearsal {
    fn rehearse(&mut self, _plan: &FixPlan) -> Result<RehearsalEvidence> {
        let outcome = self
            .link
            .ask(Req::Rehearse)
            .map_err(|detail| FixError::RehearsalFailed { detail })?;
        Ok(match outcome {
            Ok(()) => RehearsalEvidence {
                fingerprint: Fingerprint::default(),
                pre_digest: String::new(),
                post_digest: String::new(),
                invariants: InvariantReport::default(),
                passed: true,
                detail: "the remedy applied cleanly on the rehearsal snapshot".to_owned(),
            },
            Err(code) => RehearsalEvidence {
                fingerprint: Fingerprint::default(),
                pre_digest: String::new(),
                post_digest: String::new(),
                invariants: InvariantReport::default(),
                passed: false,
                detail: format!("the remedy failed on the rehearsal snapshot ({code})"),
            },
        })
    }
}

/// Verification: the state read-back, then the site's checks.
pub struct StateVerifier {
    pub(super) probe: Probe,
    pub(super) targets: Vec<String>,
    pub(super) checks: Arc<dyn FixChecks>,
}

impl FixVerifier for StateVerifier {
    fn verify(&mut self, plan: &FixPlan, changes: &[ChangeRecord]) -> Result<VerifyReport> {
        let mut violations = Vec::new();
        for change in changes {
            match self.probe.digest(&change.target) {
                Ok(current) if current == change.after => {}
                Ok(_) => violations.push(format!(
                    "`{}` is not in the state the apply left it in",
                    change.target
                )),
                Err(detail) => return Err(FixError::VerifyFailed { detail }),
            }
        }
        let outcome = self.checks.check(plan, changes);
        violations.extend(outcome.violations);
        let fingerprint = self
            .probe
            .fingerprint(&self.targets)
            .map_err(|detail| FixError::VerifyFailed { detail })?;
        Ok(VerifyReport {
            invariants: InvariantReport::default(),
            remedy_verified: violations.is_empty(),
            symptom_cleared: outcome.symptom_cleared,
            violations,
            fingerprint,
        })
    }

    fn invariants(&mut self) -> Result<InvariantReport> {
        Ok(InvariantReport::default())
    }

    fn fingerprint(&mut self) -> Result<Fingerprint> {
        self.probe
            .fingerprint(&self.targets)
            .map_err(|detail| FixError::VerifyFailed { detail })
    }
}

/// No approval inbox is hosted here: an approval is never produced.
pub struct NoApproval;

impl ApprovalPort for NoApproval {
    fn request(
        &mut self,
        _fix: &FixId,
        _plan: &FixPlan,
        _agent: &spot_fix_pipeline::AgentRef,
        _reason: &str,
    ) -> Result<Option<spot_agent_tiers::Approval>> {
        Ok(None)
    }
}

/// Every transition appended to the AI audit chain through the link; a refusal fails the run
/// closed.
pub struct LinkAudit {
    pub(super) link: Link,
}

impl FixAudit for LinkAudit {
    fn record(&mut self, event: FixAuditEvent) -> Result<()> {
        let record = event.audit_record()?;
        self.link
            .ask(|reply| Req::Audit(Box::new(record), reply))
            .map_err(|detail| FixError::AuditFailed { detail })?
            .map_err(|detail| FixError::AuditFailed { detail })
    }
}
