//! The replay budget: how many documents and how long a replay may run.
//!
//! A replay is bounded so it can always finish inside the maintenance window it is part of. When
//! the budget is reached the run stops **cleanly**, reports the coverage it reached, and leaves a
//! cursor a later run resumes from: a partial replay is a result, not a failure.
//!
//! Time comes from the [`Clock`](spot_core::Clock) the caller supplies, never from the wall clock
//! (the engine reads no clock of its own).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The bounds a replay runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// The most documents re-executed before the run stops. At least one.
    pub max_documents: u64,
    /// The longest the run may take, on the monotonic clock.
    pub max_duration: Duration,
}

impl Default for Budget {
    /// Ten thousand documents, or thirty minutes, whichever comes first.
    fn default() -> Self {
        Budget {
            max_documents: 10_000,
            max_duration: Duration::from_secs(30 * 60),
        }
    }
}

impl Budget {
    /// A budget of `max_documents` documents and `max_duration`.
    #[must_use]
    pub fn new(max_documents: u64, max_duration: Duration) -> Self {
        Budget {
            max_documents,
            max_duration,
        }
    }

    /// Refuses a budget that cannot run anything.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] when either bound is zero.
    pub fn validate(&self) -> Result<()> {
        if self.max_documents == 0 {
            return Err(Error::InvalidInput {
                what: "budget".to_owned(),
                reason: "the document cap must be at least one".to_owned(),
            });
        }
        if self.max_duration.is_zero() {
            return Err(Error::InvalidInput {
                what: "budget".to_owned(),
                reason: "the duration cap must be greater than zero".to_owned(),
            });
        }
        Ok(())
    }

    /// Whether the budget is exhausted after `documents` re-executions and `elapsed` on the
    /// monotonic clock, and which bound stopped it.
    #[must_use]
    pub fn exhausted(&self, documents: u64, elapsed: Duration) -> Option<BudgetStop> {
        if documents >= self.max_documents {
            return Some(BudgetStop::Documents);
        }
        if elapsed >= self.max_duration {
            return Some(BudgetStop::Duration);
        }
        None
    }
}

/// Which bound stopped a replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetStop {
    /// The document cap was reached.
    Documents,
    /// The duration cap was reached.
    Duration,
}

impl BudgetStop {
    /// The bound's name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetStop::Documents => "documents",
            BudgetStop::Duration => "duration",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_budget_is_refused() {
        assert!(Budget::new(0, Duration::from_secs(1)).validate().is_err());
        assert!(Budget::new(1, Duration::ZERO).validate().is_err());
        assert!(Budget::default().validate().is_ok());
    }

    #[test]
    fn the_document_cap_stops_first() {
        let budget = Budget::new(10, Duration::from_secs(60));
        assert_eq!(budget.exhausted(9, Duration::ZERO), None);
        assert_eq!(
            budget.exhausted(10, Duration::ZERO),
            Some(BudgetStop::Documents)
        );
        assert_eq!(
            budget.exhausted(0, Duration::from_secs(60)),
            Some(BudgetStop::Duration)
        );
    }
}
