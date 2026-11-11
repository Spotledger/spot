//! Deterministic fault injection: where a rescue may be killed.
//!
//! A kill stands for the process dying: the engine returns [`Error::Killed`] straight out
//! without opening the gate or cleaning anything up, and the caller then starts a fresh engine
//! over the same journal. [`NoFaults`] is the production default; [`KillAt`] kills at one chosen
//! boundary.

use crate::error::{Error, Result};

/// A boundary at which a fault-injection test may kill the rescue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KillPoint {
    /// Before the snapshot is restored.
    BeforeRestore,
    /// After the snapshot was restored and recorded, before planning.
    AfterRestore,
    /// Before the item at this zero-based position in the causal order runs.
    BeforeItem(usize),
    /// After the item at this position committed its outcome, before the next one.
    AfterItem(usize),
    /// Before the final invariant pass.
    BeforeVerify,
    /// After the final invariant pass, before the gate opens.
    AfterVerify,
    /// Before a down-patch step with this index runs.
    BeforeDownStep(usize),
    /// After the final down-patch invariant pass, before the gate opens.
    AfterDownPatches,
}

impl KillPoint {
    /// A short, stable label for the kill's error and the test log.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::BeforeRestore => "before_restore".to_owned(),
            Self::AfterRestore => "after_restore".to_owned(),
            Self::BeforeItem(index) => format!("before_item:{index}"),
            Self::AfterItem(index) => format!("after_item:{index}"),
            Self::BeforeVerify => "before_verify".to_owned(),
            Self::AfterVerify => "after_verify".to_owned(),
            Self::BeforeDownStep(index) => format!("before_down_step:{index}"),
            Self::AfterDownPatches => "after_down_patches".to_owned(),
        }
    }
}

/// Decides whether the rescue is killed at a boundary.
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

/// Kills the run at `point`.
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
