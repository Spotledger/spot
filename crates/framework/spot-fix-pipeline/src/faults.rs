//! Deterministic fault injection: where a process kill may happen.
//!
//! A kill stands for the process dying: the engine returns [`FixError::Killed`] straight out
//! without finishing the transition, and a caller restarts with a fresh engine over the same
//! journal and calls `recover`. [`NoFaults`] is the production default; [`KillAt`] kills at one
//! chosen boundary so a test can prove every journal boundary survives a crash.

use crate::error::{FixError, Result};

/// A boundary at which a fault-injection test may kill the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillPoint {
    /// Before the proposed record.
    BeforeProposed,
    /// After the proposed record, before the planned record.
    AfterProposed,
    /// Before the planned record.
    BeforePlanned,
    /// After the planned record, before an approval or the rehearsal.
    AfterPlanned,
    /// Before the rehearsal starts.
    BeforeRehearsal,
    /// After the rehearsal passed.
    AfterRehearsal,
    /// Before the apply intent is journaled.
    BeforeApply,
    /// After the apply intent, before the snapshot is taken.
    BeforeSnapshot,
    /// After the snapshot.
    AfterSnapshot,
    /// Before the pre-images are captured.
    BeforePreImages,
    /// After the pre-images and the pre-fingerprint.
    AfterPreImages,
    /// Before the step at this index runs.
    BeforeStep(usize),
    /// After the step at this index completed and was journaled.
    AfterStep(usize),
    /// After every step, before the applied record.
    BeforeApplied,
    /// After the applied record, before verification.
    AfterApplied,
    /// Before verification runs.
    BeforeVerify,
    /// After a passing verification, before the undo window opens.
    AfterVerify,
    /// Before the rollback starts.
    BeforeRollback,
    /// After the rollback's inverse ran, before the residue check.
    BeforeResidue,
    /// After a clean residue check.
    AfterRollback,
    /// Before an undo starts.
    BeforeUndo,
    /// After an undo finished.
    AfterUndo,
}

impl KillPoint {
    /// A short, stable label for the error and the test log.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::BeforeProposed => "before_proposed".to_owned(),
            Self::AfterProposed => "after_proposed".to_owned(),
            Self::BeforePlanned => "before_planned".to_owned(),
            Self::AfterPlanned => "after_planned".to_owned(),
            Self::BeforeRehearsal => "before_rehearsal".to_owned(),
            Self::AfterRehearsal => "after_rehearsal".to_owned(),
            Self::BeforeApply => "before_apply".to_owned(),
            Self::BeforeSnapshot => "before_snapshot".to_owned(),
            Self::AfterSnapshot => "after_snapshot".to_owned(),
            Self::BeforePreImages => "before_pre_images".to_owned(),
            Self::AfterPreImages => "after_pre_images".to_owned(),
            Self::BeforeStep(index) => format!("before_step:{index}"),
            Self::AfterStep(index) => format!("after_step:{index}"),
            Self::BeforeApplied => "before_applied".to_owned(),
            Self::AfterApplied => "after_applied".to_owned(),
            Self::BeforeVerify => "before_verify".to_owned(),
            Self::AfterVerify => "after_verify".to_owned(),
            Self::BeforeRollback => "before_rollback".to_owned(),
            Self::BeforeResidue => "before_residue".to_owned(),
            Self::AfterRollback => "after_rollback".to_owned(),
            Self::BeforeUndo => "before_undo".to_owned(),
            Self::AfterUndo => "after_undo".to_owned(),
        }
    }
}

/// Decides whether the run is killed at a boundary.
pub trait Faults: Send + Sync {
    /// Returns [`FixError::Killed`] to kill the run at `point`, or `Ok(())` to continue.
    ///
    /// # Errors
    ///
    /// [`FixError::Killed`] when this boundary is the chosen kill point.
    fn at(&self, point: &KillPoint) -> Result<()>;
}

/// The production default: nothing ever kills a run.
#[derive(Debug, Default)]
pub struct NoFaults;

impl Faults for NoFaults {
    fn at(&self, _point: &KillPoint) -> Result<()> {
        Ok(())
    }
}

/// Kills the run at one boundary.
#[derive(Clone, Debug)]
pub struct KillAt {
    point: KillPoint,
}

impl KillAt {
    /// A kill switch for one boundary.
    #[must_use]
    pub fn new(point: KillPoint) -> Self {
        Self { point }
    }

    /// The boundary this kills at.
    #[must_use]
    pub fn point(&self) -> &KillPoint {
        &self.point
    }
}

impl Faults for KillAt {
    fn at(&self, point: &KillPoint) -> Result<()> {
        if point == &self.point {
            Err(FixError::Killed {
                at: self.point.label(),
            })
        } else {
            Ok(())
        }
    }
}
