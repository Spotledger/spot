//! Typed errors for `spot-bpm-engine`: one enum with stable `bpm_engine.*` codes.
//!
//! Callers match on [`EngineError::code`], never on the message: the codes are this crate's
//! public vocabulary and their meaning never changes within a major version. Every code has one
//! entry in `spot-errors`' `bpm_engine.toml`, all with `class = "none"` because these errors
//! never ride an [`AppError`](spot_app_api::AppError) and are raised by the engine a host drives.

use spot_app_api::AppError;

/// Result of every fallible operation in this crate.
pub type EngineResult<T> = Result<T, EngineError>;

/// One problem while starting, stepping or cancelling a Case.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EngineError {
    /// The store does not hold a Case with this id.
    #[error("no case has id `{id}`")]
    CaseNotFound {
        /// The missing case id.
        id: String,
    },
    /// `step` was called on a Case that is no longer running.
    #[error("case `{id}` is {status}, not running")]
    CaseNotRunning {
        /// The case id.
        id: String,
        /// Its run status, spelled as the model spells it.
        status: String,
    },
    /// The compiled process a Case pins is not in the engine's registry.
    #[error("the process `{key}` version {version} is not registered")]
    ProcessNotFound {
        /// The pinned DAG key (the process name).
        key: String,
        /// The pinned version number.
        version: u32,
    },
    /// The registered process does not carry the content hash the Case pins.
    #[error("process `{key}` version {version} does not match the pinned content hash")]
    ProcessMismatch {
        /// The pinned DAG key.
        key: String,
        /// The pinned version number.
        version: u32,
    },
    /// A process version does not compile: the model reported problems.
    #[error("the process version does not compile: {detail}")]
    ProcessInvalid {
        /// A plain-language summary of the model's problems.
        detail: String,
    },
    /// No handler is registered for a node's kind.
    #[error("no handler is registered for node `{node}` of kind `{kind}`")]
    HandlerMissing {
        /// The node id.
        node: String,
        /// The node kind's token.
        kind: String,
    },
    /// A handler reported a failure that exhausted its retries.
    #[error("node `{node}` failed: {reason}")]
    HandlerFailed {
        /// The node id.
        node: String,
        /// The handler's reason, already a plain sentence.
        reason: String,
    },
    /// A running node passed its deadline.
    #[error("node `{node}` timed out after {seconds}s")]
    NodeTimeout {
        /// The node id.
        node: String,
        /// The configured timeout in whole seconds.
        seconds: u64,
    },
    /// A node's config is not the shape its kind needs.
    #[error("node `{node}` has an invalid config: {detail}")]
    ConfigInvalid {
        /// The node id.
        node: String,
        /// What the engine needed but did not find.
        detail: String,
    },
    /// A branch condition or a mapping expression does not parse or evaluate.
    #[error("node `{node}` expression `{expression}` is invalid: {detail}")]
    ExpressionInvalid {
        /// The node id.
        node: String,
        /// The offending expression text.
        expression: String,
        /// The parser or evaluator's message.
        detail: String,
    },
    /// A TypeScript code node was reached; the script host is WP-BPM-06.
    #[error("node `{node}` is a script node; the TypeScript host is not part of this engine")]
    ScriptUnsupported {
        /// The node id.
        node: String,
    },
    /// A child Case of a sub-process node failed.
    #[error("the sub-process node `{node}` failed with its child case `{child}`")]
    ChildFailed {
        /// The parent node id.
        node: String,
        /// The child case id.
        child: String,
    },
    /// A sub-process node names a process the registry does not hold.
    #[error("the sub-process node `{node}` names an unknown process `{process}`")]
    ChildProcessUnknown {
        /// The parent node id.
        node: String,
        /// The process name the node names.
        process: String,
    },
    /// The store's optimistic version did not match, so nothing was written.
    #[error("the case changed: found version {current}, expected {expected}")]
    VersionConflict {
        /// The stored version.
        current: u64,
        /// The version the caller expected.
        expected: u64,
    },
    /// The execution store failed.
    #[error("the execution store failed: {detail}")]
    StoreFailed {
        /// The backend's plain-language message.
        detail: String,
    },
    /// A scripted fault aborted the step (the simulation harness's crash injection).
    #[error("the engine crashed at {point}")]
    Crashed {
        /// The effect boundary the injected crash hit.
        point: String,
    },
    /// The Case was cancelled while a step was in flight.
    #[error("case `{id}` is cancelled")]
    Cancelled {
        /// The cancelled case id.
        id: String,
    },
    /// The caller's document work failed and nothing was committed.
    #[error("the caller's work failed: {0}")]
    WorkFailed(#[from] AppError),
}

impl EngineError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::CaseNotFound { .. } => "bpm_engine.case_not_found",
            Self::CaseNotRunning { .. } => "bpm_engine.case_not_running",
            Self::ProcessNotFound { .. } => "bpm_engine.process_not_found",
            Self::ProcessMismatch { .. } => "bpm_engine.process_mismatch",
            Self::ProcessInvalid { .. } => "bpm_engine.process_invalid",
            Self::HandlerMissing { .. } => "bpm_engine.handler_missing",
            Self::HandlerFailed { .. } => "bpm_engine.handler_failed",
            Self::NodeTimeout { .. } => "bpm_engine.node_timeout",
            Self::ConfigInvalid { .. } => "bpm_engine.config_invalid",
            Self::ExpressionInvalid { .. } => "bpm_engine.expression_invalid",
            Self::ScriptUnsupported { .. } => "bpm_engine.script_unsupported",
            Self::ChildFailed { .. } => "bpm_engine.child_failed",
            Self::ChildProcessUnknown { .. } => "bpm_engine.child_process_unknown",
            Self::VersionConflict { .. } => "bpm_engine.version_conflict",
            Self::StoreFailed { .. } => "bpm_engine.store_failed",
            Self::Crashed { .. } => "bpm_engine.crashed",
            Self::Cancelled { .. } => "bpm_engine.cancelled",
            Self::WorkFailed(_) => "bpm_engine.work_failed",
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
            EngineError::CaseNotFound { id: "c".to_owned() },
            EngineError::CaseNotRunning {
                id: "c".to_owned(),
                status: "completed".to_owned(),
            },
            EngineError::ProcessNotFound {
                key: "p".to_owned(),
                version: 1,
            },
            EngineError::ProcessMismatch {
                key: "p".to_owned(),
                version: 1,
            },
            EngineError::ProcessInvalid {
                detail: "d".to_owned(),
            },
            EngineError::HandlerMissing {
                node: "n".to_owned(),
                kind: "k".to_owned(),
            },
            EngineError::HandlerFailed {
                node: "n".to_owned(),
                reason: "r".to_owned(),
            },
            EngineError::NodeTimeout {
                node: "n".to_owned(),
                seconds: 5,
            },
            EngineError::ConfigInvalid {
                node: "n".to_owned(),
                detail: "d".to_owned(),
            },
            EngineError::ExpressionInvalid {
                node: "n".to_owned(),
                expression: "e".to_owned(),
                detail: "d".to_owned(),
            },
            EngineError::ScriptUnsupported {
                node: "n".to_owned(),
            },
            EngineError::ChildFailed {
                node: "n".to_owned(),
                child: "c".to_owned(),
            },
            EngineError::ChildProcessUnknown {
                node: "n".to_owned(),
                process: "p".to_owned(),
            },
            EngineError::VersionConflict {
                current: 2,
                expected: 1,
            },
            EngineError::StoreFailed {
                detail: "d".to_owned(),
            },
            EngineError::Crashed {
                point: "p".to_owned(),
            },
            EngineError::Cancelled { id: "c".to_owned() },
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
            assert!(code.starts_with("bpm_engine."), "{code}");
            assert!(seen.insert(code), "duplicate code {code}");
            assert!(
                catalogue.lookup(code).is_some(),
                "no catalogue entry for `{code}`"
            );
        }
        assert_eq!(seen.len(), 17, "a variant is missing from the list");
    }
}
