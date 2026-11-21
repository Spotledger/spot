//! Typed errors for `spot-fleet-rollout` (stable `fleet_rollout.*` codes).
//!
//! Callers match on [`Error::code`] and [`MigrateError::code`], never on the message. Every code
//! here has one entry in `spot-errors`' `catalogue/fleet_rollout.toml`.
//!
//! Messages never echo a secret, a document payload or a raw statement. A caller-supplied
//! `detail` is a sentence for an operator; the API documents that it must be safe to log.

/// The stable codes of this crate, catalogued in `catalogue/fleet_rollout.toml`.
pub mod codes {
    /// A node, release or wave name is not a usable identifier.
    pub const INVALID_ID: &str = "fleet_rollout.invalid_id";
    /// The plan breaks one of the structural rules.
    pub const INVALID_PLAN: &str = "fleet_rollout.invalid_plan";
    /// A wave's selectors match no node left for it.
    pub const EMPTY_WAVE: &str = "fleet_rollout.empty_wave";
    /// `resume` was called while the rollout was not paused.
    pub const NOT_PAUSED: &str = "fleet_rollout.not_paused";
    /// A terminal rollout cannot be aborted or resumed.
    pub const NOT_RUNNING: &str = "fleet_rollout.not_running";
    /// A node would move backwards in the epoch ledger.
    pub const EPOCH_REGRESSION: &str = "fleet_rollout.epoch_regression";
    /// A node cannot be recorded at an epoch the network has not reached.
    pub const EPOCH_AHEAD: &str = "fleet_rollout.epoch_ahead";
    /// The network's epoch counter is exhausted.
    pub const EPOCH_OVERFLOW: &str = "fleet_rollout.epoch_overflow";
    /// A `NodeMigrator` refused or failed the migration.
    pub const MIGRATE_FAILED: &str = "fleet_rollout.migrate_failed";
    /// The simulator reached a state it cannot explain or ran out of steps.
    pub const SIMULATION_FAILED: &str = "fleet_rollout.simulation_failed";
}

/// What can go wrong while planning or driving a rollout.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A node, release or wave name is empty, too long or contains whitespace or control
    /// characters.
    #[error("{}: {kind} is not a valid identifier: {reason}", codes::INVALID_ID)]
    InvalidId {
        /// What the value names (`node`, `release` or `wave`).
        kind: String,
        /// Why the value is unusable, without echoing it.
        reason: String,
    },
    /// The plan is structurally invalid (no waves, a misplaced canary, a duplicate name, an
    /// error-rate threshold above 1000, ...).
    #[error("{}: {reason}", codes::INVALID_PLAN)]
    InvalidPlan {
        /// The broken rule in a sentence.
        reason: String,
    },
    /// A wave selected no node that an earlier wave had not already taken.
    #[error("{}: wave {wave}", codes::EMPTY_WAVE)]
    EmptyWave {
        /// The wave's name.
        wave: String,
    },
    /// `resume` was called on a rollout that is rolling, complete or aborted.
    #[error("{}: the rollout is not paused", codes::NOT_PAUSED)]
    NotPaused,
    /// An operation needs a rollout that has not finished.
    #[error("{}: the rollout is finished", codes::NOT_RUNNING)]
    NotRunning,
    /// A node's recorded epoch would decrease.
    #[error(
        "{}: node {node} moves from epoch {previous} back to {requested}",
        codes::EPOCH_REGRESSION
    )]
    EpochRegression {
        /// The node.
        node: String,
        /// The epoch it was verified at before.
        previous: u64,
        /// The epoch the caller asked to record.
        requested: u64,
    },
    /// A node cannot be verified against an epoch newer than the network's.
    #[error(
        "{}: node {node} asked for epoch {requested} while the network is at {current}",
        codes::EPOCH_AHEAD
    )]
    EpochAhead {
        /// The node.
        node: String,
        /// The epoch the caller asked to record.
        requested: u64,
        /// The network's current epoch.
        current: u64,
    },
    /// The epoch counter cannot advance past `u64::MAX`.
    #[error("{}: the network is at epoch {current}", codes::EPOCH_OVERFLOW)]
    EpochOverflow {
        /// The current epoch.
        current: u64,
    },
    /// The simulator could not establish a rollout outcome.
    #[error("{}: {message}", codes::SIMULATION_FAILED)]
    SimulationFailed {
        /// What failed, in a sentence.
        message: String,
    },
}

impl Error {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => codes::INVALID_ID,
            Self::InvalidPlan { .. } => codes::INVALID_PLAN,
            Self::EmptyWave { .. } => codes::EMPTY_WAVE,
            Self::NotPaused => codes::NOT_PAUSED,
            Self::NotRunning => codes::NOT_RUNNING,
            Self::EpochRegression { .. } => codes::EPOCH_REGRESSION,
            Self::EpochAhead { .. } => codes::EPOCH_AHEAD,
            Self::EpochOverflow { .. } => codes::EPOCH_OVERFLOW,
            Self::SimulationFailed { .. } => codes::SIMULATION_FAILED,
        }
    }
}

/// A failure reported by a [`NodeMigrator`](crate::NodeMigrator).
///
/// The migrator owns the wording; it must be safe to log and must not carry a secret, a document
/// payload or a raw statement.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", codes::MIGRATE_FAILED)]
pub struct MigrateError {
    message: String,
}

impl MigrateError {
    /// A migration failure with an operator-facing sentence.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        codes::MIGRATE_FAILED
    }

    /// The operator-facing sentence.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code this crate can raise resolves in the compiled-in catalogue.
    #[test]
    fn every_code_is_catalogued() {
        let catalogue = spot_errors::Catalogue::load();
        let errors = [
            Error::InvalidId {
                kind: "node".to_owned(),
                reason: "it is empty".to_owned(),
            },
            Error::InvalidPlan {
                reason: "no waves".to_owned(),
            },
            Error::EmptyWave {
                wave: "canary".to_owned(),
            },
            Error::NotPaused,
            Error::NotRunning,
            Error::EpochRegression {
                node: "edge#1".to_owned(),
                previous: 2,
                requested: 1,
            },
            Error::EpochAhead {
                node: "edge#1".to_owned(),
                requested: 2,
                current: 1,
            },
            Error::EpochOverflow { current: u64::MAX },
            Error::SimulationFailed {
                message: "a later wave started".to_owned(),
            },
        ];
        for error in &errors {
            assert!(
                catalogue.lookup(error.code()).is_some(),
                "code {} has no catalogue entry",
                error.code()
            );
        }
        assert!(
            catalogue
                .lookup(MigrateError::new("seeded failure").code())
                .is_some(),
            "the migrate code has no catalogue entry"
        );
    }
}
