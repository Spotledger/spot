//! Deterministic fault injection: where a kill may happen.
//!
//! A "kill" stands for the process dying: the engine returns [`Error::Killed`] straight out
//! without opening the gate, releasing the hold or restoring anything, and the caller then
//! restarts with a fresh engine over the same journal. [`NoFaults`] is the production default;
//! the `testing` feature's `KillAt` kills at one chosen point.

use crate::error::{Error, Result};

/// A boundary at which a fault-injection test may kill the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillPoint {
    /// Before the journal record with this sequence number is written.
    BeforeJournal(u64),
    /// After the journal record with this sequence number is written, before its action.
    AfterJournal(u64),
    /// Before a step with this index runs.
    BeforeStep(usize),
    /// After a step with this index completes, before the next one.
    AfterStep(usize),
    /// Before verification runs.
    BeforeVerify,
    /// After verification passed, before the hold is released and the gate opened.
    AfterVerify,
    /// Before a rollback runs.
    BeforeRestore,
    /// After a rollback finished, before the hold is discarded and the gate opened.
    AfterRestore,
}

impl KillPoint {
    /// A short, stable label for the kill's error and the test log.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::BeforeJournal(seq) => format!("before_journal:{seq}"),
            Self::AfterJournal(seq) => format!("after_journal:{seq}"),
            Self::BeforeStep(index) => format!("before_step:{index}"),
            Self::AfterStep(index) => format!("after_step:{index}"),
            Self::BeforeVerify => "before_verify".to_owned(),
            Self::AfterVerify => "after_verify".to_owned(),
            Self::BeforeRestore => "before_restore".to_owned(),
            Self::AfterRestore => "after_restore".to_owned(),
        }
    }
}

/// Decides whether the run is killed at a boundary.
pub trait Faults: Send + Sync {
    /// Returns [`Error::Killed`] to kill the run at `point`, or `Ok(())` to continue.
    ///
    /// # Errors
    /// [`Error::Killed`] when this boundary is the chosen kill point.
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

/// Kills the run at `point` (the `testing` feature).
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
            Err(Error::Killed {
                at: self.point.label(),
            })
        } else {
            Ok(())
        }
    }
}
