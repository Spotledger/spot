//! Typed errors for `spot-bpm-triggers`: one enum with stable `bpm_triggers.*` codes.
//!
//! Callers match on [`Error::code`], never on the message: the codes are this crate's public
//! vocabulary and their meaning never changes within a major version. Every code has one entry in
//! `spot-errors`' `bpm_triggers.toml`, all with `class = "none"` because these errors are raised
//! by the trigger layer a host drives, never carried by an [`AppError`](spot_app_api::AppError).

use spot_app_api::AppError;

/// Result of every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// The stable catalogue codes this crate raises (`bpm_triggers.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerErrorCode {
    /// A trigger node's config is not complete for its kind.
    TriggerIncomplete,
    /// A document-event trigger names an operation the lifecycle does not have.
    UnknownOperation,
    /// A schedule trigger's cron expression does not parse.
    InvalidCron,
    /// A schedule trigger could not be built from its parts.
    ScheduleInvalid,
    /// No trigger kind with this name is registered.
    UnknownKind,
    /// An incoming webhook's signature did not verify.
    SignatureVerificationFailed,
    /// An incoming webhook's body is not a JSON object.
    BodyInvalid,
    /// The principal may not start through this trigger.
    PermissionDenied,
    /// The CaseStarter failed to start the Case.
    StartFailed,
    /// A manual or API start's input does not match the trigger's input schema.
    InputInvalid,
    /// A trigger condition failed to evaluate.
    ConditionEvaluationFailed,
    /// A document event's facts are incomplete, so the start is denied.
    FactsIncomplete,
}

impl TriggerErrorCode {
    /// The stable dotted code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TriggerIncomplete => "bpm_triggers.trigger_incomplete",
            Self::UnknownOperation => "bpm_triggers.unknown_operation",
            Self::InvalidCron => "bpm_triggers.invalid_cron",
            Self::ScheduleInvalid => "bpm_triggers.schedule_invalid",
            Self::UnknownKind => "bpm_triggers.unknown_kind",
            Self::SignatureVerificationFailed => "bpm_triggers.signature_verification_failed",
            Self::BodyInvalid => "bpm_triggers.body_invalid",
            Self::PermissionDenied => "bpm_triggers.permission_denied",
            Self::StartFailed => "bpm_triggers.start_failed",
            Self::InputInvalid => "bpm_triggers.input_invalid",
            Self::ConditionEvaluationFailed => "bpm_triggers.condition_evaluation_failed",
            Self::FactsIncomplete => "bpm_triggers.facts_incomplete",
        }
    }
}

/// One problem while building the trigger index, matching an event or starting a Case.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Error {
    /// A trigger node's config is missing a part its kind needs.
    #[error("trigger node `{node}` of process `{process}` is incomplete: {detail}")]
    TriggerIncomplete {
        /// The process the node belongs to.
        process: String,
        /// The trigger node.
        node: String,
        /// What the kind needed but did not find.
        detail: String,
    },
    /// A document-event trigger names an operation the lifecycle does not have.
    #[error("trigger node `{node}` names unknown operation `{operation}`")]
    UnknownOperation {
        /// The trigger node.
        node: String,
        /// The operation text the node carried.
        operation: String,
    },
    /// A schedule trigger's cron expression does not parse.
    #[error("schedule trigger `{node}` has an invalid cron expression: {detail}")]
    InvalidCron {
        /// The trigger node.
        node: String,
        /// The parser's message.
        detail: String,
    },
    /// A schedule trigger could not be built from its parts.
    #[error("schedule trigger `{node}` could not be built: {detail}")]
    ScheduleInvalid {
        /// The trigger node.
        node: String,
        /// The builder's message.
        detail: String,
    },
    /// No trigger kind with this name is registered.
    #[error("no trigger kind `{kind}` is registered")]
    UnknownKind {
        /// The kind token.
        kind: String,
    },
    /// An incoming webhook's signature did not verify.
    #[error("webhook signature verification failed: {reason}")]
    SignatureVerificationFailed {
        /// Why (never the secret or the body).
        reason: String,
    },
    /// An incoming webhook's body is not a JSON object.
    #[error("the webhook body is not a JSON object: {reason}")]
    BodyInvalid {
        /// Why.
        reason: String,
    },
    /// The principal may not start through this trigger.
    #[error("the principal may not start `{process}` through trigger node `{node}`")]
    PermissionDenied {
        /// The process the trigger belongs to.
        process: String,
        /// The trigger node.
        node: String,
    },
    /// The `CaseStarter` failed to start the Case.
    #[error("starting the Case for `{process}` node `{node}` failed: {reason}")]
    StartFailed {
        /// The process the trigger belongs to.
        process: String,
        /// The trigger node.
        node: String,
        /// The starter's plain-language message.
        reason: String,
    },
    /// A manual or API start's input does not match the trigger's input schema.
    #[error("the start input for `{process}` node `{node}` is invalid: {problems:?}")]
    InputInvalid {
        /// The process the trigger belongs to.
        process: String,
        /// The trigger node.
        node: String,
        /// Every problem found: a JSON pointer and a fault kind, never the caller's value.
        problems: Vec<String>,
    },
    /// A trigger condition failed to evaluate.
    #[error("trigger condition `{condition}` failed to evaluate: {reason}")]
    ConditionEvaluationFailed {
        /// The condition text.
        condition: String,
        /// The evaluator's message.
        reason: String,
    },
    /// A document event's facts are incomplete (no object image, no scalar values, or no state
    /// for a stateful doctype), so a person's start is denied rather than judged on nothing.
    #[error("the facts of a `{doctype}` document event are incomplete: {reason}")]
    FactsIncomplete {
        /// The doctype of the event's document.
        doctype: String,
        /// What is missing.
        reason: String,
    },
}

impl Error {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> TriggerErrorCode {
        match self {
            Self::TriggerIncomplete { .. } => TriggerErrorCode::TriggerIncomplete,
            Self::UnknownOperation { .. } => TriggerErrorCode::UnknownOperation,
            Self::InvalidCron { .. } => TriggerErrorCode::InvalidCron,
            Self::ScheduleInvalid { .. } => TriggerErrorCode::ScheduleInvalid,
            Self::UnknownKind { .. } => TriggerErrorCode::UnknownKind,
            Self::SignatureVerificationFailed { .. } => {
                TriggerErrorCode::SignatureVerificationFailed
            }
            Self::BodyInvalid { .. } => TriggerErrorCode::BodyInvalid,
            Self::PermissionDenied { .. } => TriggerErrorCode::PermissionDenied,
            Self::StartFailed { .. } => TriggerErrorCode::StartFailed,
            Self::InputInvalid { .. } => TriggerErrorCode::InputInvalid,
            Self::ConditionEvaluationFailed { .. } => TriggerErrorCode::ConditionEvaluationFailed,
            Self::FactsIncomplete { .. } => TriggerErrorCode::FactsIncomplete,
        }
    }

    /// The app-API error this maps to. Every code is `none`, so the class is `Internal` and the
    /// host re-classifies where it needs to.
    #[must_use]
    pub fn to_app_error(&self) -> AppError {
        AppError::new(
            spot_app_api::ErrorClass::Internal,
            self.code().as_str(),
            self.to_string(),
        )
    }
}

impl From<spot_webhooks::Error> for Error {
    fn from(e: spot_webhooks::Error) -> Self {
        Error::SignatureVerificationFailed {
            reason: e.to_string(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn every_error() -> Vec<Error> {
        vec![
            Error::TriggerIncomplete {
                process: "p".to_owned(),
                node: "n".to_owned(),
                detail: "d".to_owned(),
            },
            Error::UnknownOperation {
                node: "n".to_owned(),
                operation: "o".to_owned(),
            },
            Error::InvalidCron {
                node: "n".to_owned(),
                detail: "d".to_owned(),
            },
            Error::ScheduleInvalid {
                node: "n".to_owned(),
                detail: "d".to_owned(),
            },
            Error::UnknownKind {
                kind: "k".to_owned(),
            },
            Error::SignatureVerificationFailed {
                reason: "r".to_owned(),
            },
            Error::BodyInvalid {
                reason: "r".to_owned(),
            },
            Error::PermissionDenied {
                process: "p".to_owned(),
                node: "n".to_owned(),
            },
            Error::StartFailed {
                process: "p".to_owned(),
                node: "n".to_owned(),
                reason: "r".to_owned(),
            },
            Error::InputInvalid {
                process: "p".to_owned(),
                node: "n".to_owned(),
                problems: vec!["/a: required and missing".to_owned()],
            },
            Error::ConditionEvaluationFailed {
                condition: "c".to_owned(),
                reason: "r".to_owned(),
            },
            Error::FactsIncomplete {
                doctype: "d".to_owned(),
                reason: "r".to_owned(),
            },
        ]
    }

    #[test]
    fn every_code_has_a_catalogue_entry() {
        let catalogue = spot_errors::Catalogue::load();
        assert_eq!(
            catalogue.load_error(),
            None,
            "the catalogue must load clean"
        );
        let mut seen = std::collections::BTreeSet::new();
        for error in &every_error() {
            let code = error.code().as_str();
            assert!(code.starts_with("bpm_triggers."), "{code}");
            assert!(spot_app_api::error::is_code(code), "{code} is not dotted");
            assert!(seen.insert(code), "duplicate code {code}");
            assert!(
                catalogue.lookup(code).is_some(),
                "no catalogue entry for `{code}`"
            );
        }
        assert_eq!(
            seen.len(),
            12,
            "a variant is missing from the catalogue test"
        );
    }
}
