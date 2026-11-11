//! The invariant seam that must pass before users return.
//!
//! The invariant definitions themselves belong to WP-TST-07's L7 suite; this crate owns the
//! call sites and the fail-closed rule. A rescue checks invariants **after the restore and
//! before any re-entry** (so the old version is sound to write against) and again **after the
//! last item** (so the re-entered state is sound to admit users to). The maintenance gate is
//! opened only after the second pass returns no violation.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::error::Result;

/// Which point of the rescue an invariant pass follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvariantPhase {
    /// Immediately after the pre-migration snapshot was restored.
    AfterRestore,
    /// Immediately after every down-patch ran and before users return.
    AfterDownPatches,
    /// Immediately after every re-entrant item ran and before users return.
    AfterReentry,
}

impl InvariantPhase {
    /// A short, stable label for a report or log line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::AfterRestore => "after_restore",
            Self::AfterDownPatches => "after_down_patches",
            Self::AfterReentry => "after_reentry",
        }
    }
}

/// One invariant and whether it held.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantCheck {
    /// The invariant's stable name.
    pub name: String,
    /// Whether it held.
    pub passed: bool,
    /// A secret-free explanation of a failure (empty when it held).
    pub detail: String,
}

impl InvariantCheck {
    /// A passing check.
    #[must_use]
    pub fn passed(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: true,
            detail: String::new(),
        }
    }

    /// A failing check.
    #[must_use]
    pub fn failed(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: false,
            detail: detail.into(),
        }
    }
}

/// The result of one invariant pass.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantReport {
    /// The phase that was checked.
    pub phase: Option<InvariantPhase>,
    /// One entry per invariant checked.
    pub checks: Vec<InvariantCheck>,
}

impl InvariantReport {
    /// A report for `phase` over `checks`.
    #[must_use]
    pub fn new(phase: InvariantPhase, checks: Vec<InvariantCheck>) -> Self {
        Self {
            phase: Some(phase),
            checks,
        }
    }

    /// Whether every invariant held.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.passed)
    }

    /// The failing checks.
    #[must_use]
    pub fn violations(&self) -> Vec<&InvariantCheck> {
        self.checks.iter().filter(|check| !check.passed).collect()
    }

    /// The number of violations.
    #[must_use]
    pub fn violation_count(&self) -> usize {
        self.violations().len()
    }
}

/// The invariants a host registers for a rescue.
///
/// A set checks every invariant and returns all violations; it never stops at the first one.
/// The host builds the set from WP-TST-07's definitions and the site's own invariant list.
pub trait Invariants: Send + Sync {
    /// Checks every invariant for `phase`.
    ///
    /// # Errors
    /// A backend error when the state cannot be read. A failing invariant is a report with a
    /// failing check, not an error.
    fn check(&self, phase: InvariantPhase) -> BoxFuture<'_, Result<InvariantReport>>;
}

/// An invariant set that records its calls and returns a fixed result.
///
/// It is the reference double for tests and for a host before the real L7 set is wired.
#[derive(Debug)]
pub struct RecordingInvariants {
    report: Mutex<InvariantReport>,
    phases: Mutex<Vec<InvariantPhase>>,
}

impl RecordingInvariants {
    /// A set that passes `checks` at every phase.
    #[must_use]
    pub fn passing(checks: Vec<InvariantCheck>) -> Self {
        Self {
            report: Mutex::new(InvariantReport {
                phase: None,
                checks,
            }),
            phases: Mutex::new(Vec::new()),
        }
    }

    /// A set that fails at every phase with the given checks.
    #[must_use]
    pub fn failing(checks: Vec<InvariantCheck>) -> Self {
        Self::passing(checks)
    }

    /// The phases this set was asked about, in order.
    #[must_use]
    pub fn phases(&self) -> Vec<InvariantPhase> {
        self.phases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Invariants for RecordingInvariants {
    fn check(&self, phase: InvariantPhase) -> BoxFuture<'_, Result<InvariantReport>> {
        Box::pin(async move {
            self.phases
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(phase);
            let mut report = self
                .report
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            report.phase = Some(phase);
            Ok(report)
        })
    }
}
