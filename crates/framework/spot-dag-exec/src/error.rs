//! Typed errors for `spot-dag-exec`: one enum with stable `dag_exec.*` codes.
//!
//! Callers match on [`ExecError::code`], never on the message: the codes are this crate's
//! public vocabulary and their meaning never changes within a major version. Every code has
//! one entry in `spot-errors`' `dag_exec.toml`, all with `class = "none"` because these errors
//! never ride an [`AppError`] and are raised by the engine that consumes this crate.

use spot_app_api::AppError;
use spot_storage_contract::StorageError;

/// Result of every fallible operation in this crate.
pub type ExecResult<T> = Result<T, ExecError>;

/// One problem with a run, a transition or a store operation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ExecError {
    /// A run id does not match `[a-z][a-z0-9_]{0,63}`.
    #[error("invalid run id `{raw}`: must match [a-z][a-z0-9_]{{0,63}}")]
    RunIdInvalid {
        /// The rejected id text.
        raw: String,
    },
    /// The business key is empty or longer than 256 characters.
    #[error("the business key must be 1 to 256 characters")]
    KeyInvalid,
    /// `create_run` was given an id a run already uses.
    #[error("a run with id `{id}` already exists")]
    RunExists {
        /// The duplicated id.
        id: String,
    },
    /// `create_run` was given a business key a run already uses.
    #[error("a run with business key `{key}` already exists")]
    KeyConflict {
        /// The duplicated key.
        key: String,
    },
    /// `load_run`/`apply` named a run the store does not hold.
    #[error("no run has id `{id}`")]
    RunNotFound {
        /// The missing id.
        id: String,
    },
    /// `apply` was given a version that is not the stored one.
    #[error("run changed: found version {current}, expected {expected}")]
    VersionConflict {
        /// The current stored version.
        current: u64,
        /// The version the caller expected.
        expected: u64,
    },
    /// A transition names a node the run or its DAG does not have.
    #[error("the run does not track node `{node}`")]
    InvalidNode {
        /// The unknown node id.
        node: String,
    },
    /// A transition is not allowed in the run's or node's current state.
    #[error("transition not allowed: {detail}")]
    InvalidState {
        /// A plain-language description of the state the caller got wrong.
        detail: String,
    },
    /// The compiled DAG does not match the published version the run pins.
    #[error("DAG mismatch: found `{current}`, expected `{expected}`")]
    DagMismatch {
        /// What the compiled DAG carries.
        current: String,
        /// What the published pin carries.
        expected: String,
    },
    /// A node output payload carries a float where only integers are allowed (ADR-12).
    #[error("output payload is not canonical JSON: a float appears at `{path}`")]
    OutputInvalid {
        /// Path to the float inside the payload, for example `items[0].qty`.
        path: String,
    },
    /// A stored instant is not a valid UTC timestamp.
    #[error("invalid instant: {seconds}s {nanos}ns")]
    TimeInvalid {
        /// Seconds since the Unix epoch.
        seconds: i64,
        /// Nanoseconds within the second.
        nanos: u32,
    },
    /// The caller's document work failed inside `commit_with`; nothing was stored.
    #[error("the caller's work failed: {0}")]
    WorkFailed(#[from] AppError),
    /// The storage backend failed while opening or committing the unit of work.
    #[error("storage failed: {0}")]
    StorageFailed(#[from] StorageError),
}

impl ExecError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RunIdInvalid { .. } => "dag_exec.run_id_invalid",
            Self::KeyInvalid => "dag_exec.key_invalid",
            Self::RunExists { .. } => "dag_exec.run_exists",
            Self::KeyConflict { .. } => "dag_exec.key_conflict",
            Self::RunNotFound { .. } => "dag_exec.run_not_found",
            Self::VersionConflict { .. } => "dag_exec.version_conflict",
            Self::InvalidNode { .. } => "dag_exec.invalid_node",
            Self::InvalidState { .. } => "dag_exec.invalid_state",
            Self::DagMismatch { .. } => "dag_exec.dag_mismatch",
            Self::OutputInvalid { .. } => "dag_exec.output_invalid",
            Self::TimeInvalid { .. } => "dag_exec.time_invalid",
            Self::WorkFailed(_) => "dag_exec.work_failed",
            Self::StorageFailed(_) => "dag_exec.storage_failed",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_unique() {
        let errors = [
            ExecError::RunIdInvalid {
                raw: "X".to_owned(),
            },
            ExecError::KeyInvalid,
            ExecError::RunExists { id: "r".to_owned() },
            ExecError::KeyConflict {
                key: "k".to_owned(),
            },
            ExecError::RunNotFound { id: "r".to_owned() },
            ExecError::VersionConflict {
                current: 2,
                expected: 1,
            },
            ExecError::InvalidNode {
                node: "n".to_owned(),
            },
            ExecError::InvalidState {
                detail: "x".to_owned(),
            },
            ExecError::DagMismatch {
                current: "a".to_owned(),
                expected: "b".to_owned(),
            },
            ExecError::OutputInvalid {
                path: "a".to_owned(),
            },
            ExecError::TimeInvalid {
                seconds: 0,
                nanos: 0,
            },
        ];
        let mut seen = std::collections::BTreeSet::new();
        for error in &errors {
            let code = error.code();
            assert!(code.starts_with("dag_exec."), "{code}");
            assert!(seen.insert(code), "duplicate code {code}");
        }
        assert_eq!(seen.len(), 11);
    }

    #[test]
    fn every_code_has_a_catalogue_entry() {
        let errors = vec![
            ExecError::RunIdInvalid {
                raw: "X".to_owned(),
            },
            ExecError::KeyInvalid,
            ExecError::RunExists { id: "r".to_owned() },
            ExecError::KeyConflict {
                key: "k".to_owned(),
            },
            ExecError::RunNotFound { id: "r".to_owned() },
            ExecError::VersionConflict {
                current: 2,
                expected: 1,
            },
            ExecError::InvalidNode {
                node: "n".to_owned(),
            },
            ExecError::InvalidState {
                detail: "x".to_owned(),
            },
            ExecError::DagMismatch {
                current: "a".to_owned(),
                expected: "b".to_owned(),
            },
            ExecError::OutputInvalid {
                path: "a".to_owned(),
            },
            ExecError::TimeInvalid {
                seconds: 0,
                nanos: 0,
            },
            ExecError::WorkFailed(spot_app_api::AppError::new(
                spot_app_api::ErrorClass::Internal,
                "test.probe",
                "probe",
            )),
            ExecError::StorageFailed(spot_storage_contract::StorageError::new(
                spot_storage_contract::ErrorKind::Internal,
                "probe",
            )),
        ];
        let catalogue = spot_errors::Catalogue::load();
        assert_eq!(
            catalogue.load_error(),
            None,
            "the catalogue must load clean"
        );
        let mut seen = std::collections::BTreeSet::new();
        for error in &errors {
            let code = error.code();
            assert!(seen.insert(code), "duplicate code {code}");
            assert!(
                catalogue.lookup(code).is_some(),
                "no catalogue entry for `{code}`"
            );
        }
        assert_eq!(seen.len(), 13, "a variant is missing from the list");
    }
}
