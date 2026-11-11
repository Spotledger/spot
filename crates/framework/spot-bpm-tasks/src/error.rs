//! Typed errors for `spot-bpm-tasks`: one enum with stable `bpm_tasks.*` codes.
//!
//! Callers match on [`TasksError::code`], never on the message: the codes are this crate's
//! public vocabulary and their meaning never changes within a major version. Every code has one
//! entry in `spot-errors`' `bpm_tasks.toml`, all with `class = "none"` because these errors
//! never ride an [`AppError`](spot_app_api::AppError): they are raised by the task service a
//! host drives, and the host decides how to surface them.

/// Result of every fallible operation in this crate.
pub type TasksResult<T> = Result<T, TasksError>;

/// One problem while creating, acting on or querying a task.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TasksError {
    /// No assignee could be resolved for a Decision node: the Case parks with this code rather
    /// than guessing a person.
    #[error("no assignee could be resolved for the decision node `{node}`")]
    NoAssignee {
        /// The node whose assignee is unresolved.
        node: String,
    },
    /// The store does not hold a task with this id.
    #[error("no task has id `{id}`")]
    TaskNotFound {
        /// The missing task id.
        id: String,
    },
    /// The task is no longer open, so the action is refused.
    #[error("task `{id}` is {status}, not open")]
    TaskNotOpen {
        /// The task id.
        id: String,
        /// Its status, spelled as this crate spells it.
        status: String,
    },
    /// The acting user is neither an assignee nor an active delegate of the task.
    #[error("user `{user}` may not act on task `{task}`")]
    NotAssignee {
        /// The task id.
        task: String,
        /// The acting user.
        user: String,
    },
    /// The acting user is not a supervisor of the task's assignees.
    #[error("user `{user}` may not supervise task `{task}`")]
    NotSupervisor {
        /// The task id.
        task: String,
        /// The acting user.
        user: String,
    },
    /// The chosen outcome is not one of the task's options.
    #[error("task `{task}` does not offer the choice `{choice}`")]
    InvalidChoice {
        /// The task id.
        task: String,
        /// The choice, as text.
        choice: String,
    },
    /// The node says the decision must be distinct from the document's creator, and it is not.
    #[error("task `{task}` must be decided by someone other than `{user}`")]
    DistinctFromCreator {
        /// The task id.
        task: String,
        /// The user who created the underlying document and tried to decide.
        user: String,
    },
    /// Completing the task would breach a segregation-of-duties rule.
    #[error("task `{task}` is blocked for `{user}`: {detail}")]
    SegregationBlocked {
        /// The task id.
        task: String,
        /// The acting user.
        user: String,
        /// A plain-language summary of the conflicting rule.
        detail: String,
    },
    /// A delegate tried to act after its delegation expired.
    #[error("the delegation of task `{task}` to `{delegate}` has expired")]
    DelegationExpired {
        /// The task id.
        task: String,
        /// The delegate whose delegation expired.
        delegate: String,
    },
    /// The task changed since it was read, so nothing was written.
    #[error("task `{id}` changed: found version {current}, expected {expected}")]
    VersionConflict {
        /// The task id.
        id: String,
        /// The stored version.
        current: u64,
        /// The version the caller expected.
        expected: u64,
    },
    /// The task store failed.
    #[error("the task store failed: {detail}")]
    StoreFailed {
        /// The backend's plain-language message.
        detail: String,
    },
    /// The engine failed to resume the Case after a completion.
    #[error("the engine could not resume the Case: {detail}")]
    EngineFailed {
        /// The engine's plain-language message.
        detail: String,
    },
    /// A Decision node's config is not the shape its kind needs.
    #[error("decision node `{node}` has an invalid config: {detail}")]
    ConfigInvalid {
        /// The node id.
        node: String,
        /// What the handler needed but did not find.
        detail: String,
    },
    /// The SLA's final action is `auto-approve`, but the node does not declare
    /// `sla_auto_approve = true`.
    #[error("node `{node}` would approve automatically on timeout but does not declare it")]
    AutoApproveUndeclared {
        /// The node id.
        node: String,
    },
    /// The SLA's final action is `auto-approve` on a node that is segregated (it requires a
    /// distinct decider or declares a duty).
    #[error("node `{node}` is segregated and cannot approve automatically")]
    AutoApproveWithSegregation {
        /// The node id.
        node: String,
    },
}

impl TasksError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoAssignee { .. } => "bpm_tasks.no_assignee",
            Self::TaskNotFound { .. } => "bpm_tasks.task_not_found",
            Self::TaskNotOpen { .. } => "bpm_tasks.task_not_open",
            Self::NotAssignee { .. } => "bpm_tasks.not_assignee",
            Self::NotSupervisor { .. } => "bpm_tasks.not_supervisor",
            Self::InvalidChoice { .. } => "bpm_tasks.invalid_choice",
            Self::DistinctFromCreator { .. } => "bpm_tasks.distinct_from_creator",
            Self::SegregationBlocked { .. } => "bpm_tasks.sod_conflict",
            Self::DelegationExpired { .. } => "bpm_tasks.delegation_expired",
            Self::VersionConflict { .. } => "bpm_tasks.version_conflict",
            Self::StoreFailed { .. } => "bpm_tasks.store_failed",
            Self::EngineFailed { .. } => "bpm_tasks.engine_failed",
            Self::ConfigInvalid { .. } => "bpm_tasks.config_invalid",
            Self::AutoApproveUndeclared { .. } => "bpm_tasks.auto_approve_undeclared",
            Self::AutoApproveWithSegregation { .. } => "bpm_tasks.auto_approve_with_segregation",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_a_catalogue_entry() {
        let errors = vec![
            TasksError::NoAssignee {
                node: "n".to_owned(),
            },
            TasksError::TaskNotFound { id: "t".to_owned() },
            TasksError::TaskNotOpen {
                id: "t".to_owned(),
                status: "completed".to_owned(),
            },
            TasksError::NotAssignee {
                task: "t".to_owned(),
                user: "u".to_owned(),
            },
            TasksError::NotSupervisor {
                task: "t".to_owned(),
                user: "u".to_owned(),
            },
            TasksError::InvalidChoice {
                task: "t".to_owned(),
                choice: "c".to_owned(),
            },
            TasksError::DistinctFromCreator {
                task: "t".to_owned(),
                user: "u".to_owned(),
            },
            TasksError::SegregationBlocked {
                task: "t".to_owned(),
                user: "u".to_owned(),
                detail: "d".to_owned(),
            },
            TasksError::DelegationExpired {
                task: "t".to_owned(),
                delegate: "u".to_owned(),
            },
            TasksError::VersionConflict {
                id: "t".to_owned(),
                current: 2,
                expected: 1,
            },
            TasksError::StoreFailed {
                detail: "d".to_owned(),
            },
            TasksError::EngineFailed {
                detail: "d".to_owned(),
            },
            TasksError::ConfigInvalid {
                node: "n".to_owned(),
                detail: "d".to_owned(),
            },
            TasksError::AutoApproveUndeclared {
                node: "n".to_owned(),
            },
            TasksError::AutoApproveWithSegregation {
                node: "n".to_owned(),
            },
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
            assert!(code.starts_with("bpm_tasks."), "{code}");
            assert!(seen.insert(code), "duplicate code {code}");
            assert!(
                catalogue.lookup(code).is_some(),
                "no catalogue entry for `{code}`"
            );
        }
        assert_eq!(seen.len(), 15, "a variant is missing from the list");
    }
}
