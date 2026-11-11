//! [`ReencryptPlan`]: the resumable batch plan a rotation produces.
//!
//! Rotation leaves old epochs readable. Re-encrypting old data under the new epoch is a
//! separate, resumable job (ADR-070 rule 12.13 e): the hub hands out a plan of batches, a
//! worker runs a batch, records how many it finished, and resumes from the next one after a
//! restart. This crate plans the batches; it runs no job.

use crate::ids::{GroupName, KeyEpoch};
use crate::time::MonoTime;
use serde::{Deserialize, Serialize};

/// One epoch to re-encrypt into the plan's target epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReencryptBatch {
    /// The protection group.
    pub group: GroupName,
    /// The epoch whose data is read.
    pub from: KeyEpoch,
    /// The epoch data is written under.
    pub to: KeyEpoch,
}

/// A resumable plan to re-encrypt a group's older epochs under the current one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReencryptPlan {
    /// The protection group.
    pub group: GroupName,
    /// The epoch data is written under.
    pub target: KeyEpoch,
    /// The batches, oldest epoch first.
    pub batches: Vec<ReencryptBatch>,
    /// When the plan was produced, on the hub's monotonic clock.
    pub created_at: MonoTime,
}

impl ReencryptPlan {
    /// The number of batches.
    #[must_use]
    pub fn len(&self) -> usize {
        self.batches.len()
    }

    /// Whether there is nothing to re-encrypt.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    /// The batch after `done` finished batches, or `None` when the plan is complete.
    #[must_use]
    pub fn resume(&self, done: usize) -> Option<&ReencryptBatch> {
        self.batches.get(done)
    }

    /// Whether `done` finished batches complete the plan.
    #[must_use]
    pub fn is_complete(&self, done: usize) -> bool {
        done >= self.batches.len()
    }

    /// The epochs still to be re-encrypted after `done` finished batches.
    #[must_use]
    pub fn remaining(&self, done: usize) -> usize {
        self.batches.len().saturating_sub(done)
    }
}
