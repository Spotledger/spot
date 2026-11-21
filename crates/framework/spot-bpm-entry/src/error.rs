//! The one error type of this crate, with stable `bpm_entry.*` codes.
//!
//! Messages are plain language and name no secret, no raw value and no internal run word: a
//! person reads them, so they say "request" through the label pair where they name a run at all
//! (ADR-080 section 3).

use spot_app_api::{AppError, ErrorClass, FieldPath};

use crate::codes;

/// One entered value the record type's validation refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldIssue {
    /// The field, as the form shows it.
    pub field: String,
    /// The stable code the validation gave.
    pub code: String,
    /// The plain-language reason.
    pub message: String,
}

impl FieldIssue {
    /// A field issue.
    #[must_use]
    pub fn new(field: &str, code: &str, message: &str) -> Self {
        Self {
            field: field.to_owned(),
            code: code.to_owned(),
            message: message.to_owned(),
        }
    }
}

/// Everything this crate can refuse or fail with.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EntryError {
    /// A draft owned by a running request was changed from another channel.
    #[error(
        "This `{doctype}` is being prepared in request `{request}` and can be changed only \
         there."
    )]
    OwnedByRequest {
        /// The record type.
        doctype: String,
        /// The owning request, as its link target.
        request: String,
    },

    /// A `process_only` record type was created outside its entry process.
    #[error(
        "`{doctype}` can only be created by starting the process `{process}`; start it instead \
         of creating the record directly."
    )]
    StartProcess {
        /// The record type.
        doctype: String,
        /// The entry process to start.
        process: String,
    },

    /// The scope a create names is unknown, or the person may not create in it.
    #[error("`{doctype}` cannot be created in the named company or unit.")]
    ScopeInvalid {
        /// The record type.
        doctype: String,
    },

    /// A configuration did not parse or is incomplete.
    #[error("The configuration of `{node}` is not valid: {detail}")]
    ConfigInvalid {
        /// The node or setting.
        node: String,
        /// What is wrong.
        detail: String,
    },

    /// The entered values failed the record type's validation.
    #[error("The entered values were not accepted; correct the marked fields and try again.")]
    FormInvalid {
        /// Every field issue, in the order found.
        fields: Vec<FieldIssue>,
    },

    /// A draft could not be written through its lifecycle.
    #[error("The draft could not be saved: {detail}")]
    DraftFailed {
        /// What happened.
        detail: String,
    },

    /// The step's task is not open, or the person may not act on it.
    #[error("The step `{task}` cannot be completed now: {detail}")]
    StepNotOpen {
        /// The task id.
        task: String,
        /// Why.
        detail: String,
    },

    /// The step resolved to nobody to ask.
    #[error("The step `{node}` has nobody to ask.")]
    NoAssignee {
        /// The node id.
        node: String,
    },

    /// The task inbox failed.
    #[error("The task inbox is unavailable: {detail}")]
    Inbox {
        /// What happened.
        detail: String,
    },

    /// The step transition and the draft write did not commit together.
    #[error("The step and its draft could not be saved together; nothing was stored: {detail}")]
    CommitFailed {
        /// What happened.
        detail: String,
    },

    /// A batch run is in a state that does not allow the call.
    #[error("The batch `{run}` cannot do that now: {detail}")]
    BatchState {
        /// The run id.
        run: String,
        /// Why.
        detail: String,
    },

    /// A batch run reached its failure limit.
    #[error("The batch `{run}` stopped: it reached its limit of {limit} problem items.")]
    BatchLimit {
        /// The run id.
        run: String,
        /// The configured limit.
        limit: usize,
    },

    /// A batch run could not be started, stored or read.
    #[error("The batch could not be handled: {detail}")]
    BatchFailed {
        /// What happened.
        detail: String,
    },

    /// A setting could not be read.
    #[error("The entry settings are unavailable: {detail}")]
    Settings {
        /// What happened.
        detail: String,
    },

    /// An audit entry could not be written.
    #[error("The audit entry could not be written: {detail}")]
    Audit {
        /// What happened.
        detail: String,
    },

    /// A label override was blank.
    #[error("The {which} label must not be blank.")]
    LabelInvalid {
        /// `singular` or `plural`.
        which: String,
    },
}

/// The result type of this crate.
pub type EntryResult<T> = Result<T, EntryError>;

impl EntryError {
    /// The stable `bpm_entry.*` code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OwnedByRequest { .. } => codes::OWNED_BY_REQUEST,
            Self::StartProcess { .. } => codes::START_PROCESS,
            Self::ScopeInvalid { .. } => codes::SCOPE_INVALID,
            Self::ConfigInvalid { .. } => codes::CONFIG_INVALID,
            Self::FormInvalid { .. } => codes::FORM_INVALID,
            Self::DraftFailed { .. } => codes::DRAFT_FAILED,
            Self::StepNotOpen { .. } => codes::STEP_NOT_OPEN,
            Self::NoAssignee { .. } => codes::NO_ASSIGNEE,
            Self::Inbox { .. } => codes::INBOX,
            Self::CommitFailed { .. } => codes::COMMIT_FAILED,
            Self::BatchState { .. } => codes::BATCH_STATE,
            Self::BatchLimit { .. } => codes::BATCH_LIMIT,
            Self::BatchFailed { .. } => codes::BATCH_FAILED,
            Self::Settings { .. } => codes::SETTINGS,
            Self::Audit { .. } => codes::AUDIT,
            Self::LabelInvalid { .. } => codes::LABEL_INVALID,
        }
    }

    /// How the error is reported.
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::OwnedByRequest { .. }
            | Self::StartProcess { .. }
            | Self::ScopeInvalid { .. }
            | Self::ConfigInvalid { .. }
            | Self::FormInvalid { .. }
            | Self::NoAssignee { .. }
            | Self::BatchLimit { .. }
            | Self::LabelInvalid { .. } => ErrorClass::Validation,
            Self::StepNotOpen { .. } | Self::BatchState { .. } => ErrorClass::InvalidState,
            Self::DraftFailed { .. } | Self::CommitFailed { .. } | Self::BatchFailed { .. } => {
                ErrorClass::Internal
            }
            Self::Inbox { .. } | Self::Audit { .. } | Self::Settings { .. } => {
                ErrorClass::Unavailable
            }
        }
    }

    /// The error as the app API carries it: field errors for a refused form, and the details a
    /// caller needs to act (the entry process to start, the owning request to open).
    #[must_use]
    pub fn to_app_error(&self) -> AppError {
        let mut error = AppError::new(self.class(), self.code(), self.to_string());
        match self {
            Self::StartProcess { doctype, process } => {
                error = error
                    .detail("record_type", doctype.as_str())
                    .detail("process", process.as_str());
            }
            Self::OwnedByRequest { doctype, request } => {
                error = error
                    .detail("record_type", doctype.as_str())
                    .detail("request", request.as_str());
            }
            Self::FormInvalid { fields } => {
                for issue in fields {
                    if let Some(path) = FieldPath::parse(&issue.field) {
                        error = error.with_field(path, &issue.code, issue.message.clone());
                    }
                }
            }
            _ => {}
        }
        error
    }
}

impl From<spot_bpm_governance::GovernanceError> for EntryError {
    fn from(error: spot_bpm_governance::GovernanceError) -> Self {
        match error {
            spot_bpm_governance::GovernanceError::Audit { detail } => Self::Audit { detail },
            other => Self::Settings {
                detail: other.to_string(),
            },
        }
    }
}

impl From<EntryError> for AppError {
    fn from(error: EntryError) -> Self {
        error.to_app_error()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<EntryError> {
        let s = |text: &str| text.to_owned();
        vec![
            EntryError::OwnedByRequest {
                doctype: s("d"),
                request: s("r"),
            },
            EntryError::StartProcess {
                doctype: s("d"),
                process: s("p"),
            },
            EntryError::ConfigInvalid {
                node: s("n"),
                detail: s("d"),
            },
            EntryError::FormInvalid {
                fields: vec![FieldIssue::new("name", "x.y", "m")],
            },
            EntryError::DraftFailed { detail: s("d") },
            EntryError::StepNotOpen {
                task: s("t"),
                detail: s("d"),
            },
            EntryError::ScopeInvalid { doctype: s("d") },
            EntryError::NoAssignee { node: s("n") },
            EntryError::Inbox { detail: s("d") },
            EntryError::CommitFailed { detail: s("d") },
            EntryError::BatchState {
                run: s("r"),
                detail: s("d"),
            },
            EntryError::BatchLimit {
                run: s("r"),
                limit: 3,
            },
            EntryError::BatchFailed { detail: s("d") },
            EntryError::Settings { detail: s("d") },
            EntryError::Audit { detail: s("d") },
            EntryError::LabelInvalid {
                which: s("singular"),
            },
        ]
    }

    #[test]
    fn every_code_is_catalogued_with_its_class() {
        let catalogue = spot_errors::Catalogue::load();
        assert_eq!(
            catalogue.load_error(),
            None,
            "the catalogue must load clean"
        );
        let mut seen = std::collections::BTreeSet::new();
        for error in every_variant() {
            let code = error.code();
            assert!(code.starts_with("bpm_entry."), "{code}");
            assert!(seen.insert(code), "duplicate code {code}");
            let entry = catalogue.lookup(code);
            assert!(entry.is_some(), "no catalogue entry for `{code}`");
            assert!(
                !catalogue.missing_explanation(&error.to_app_error()),
                "{code}"
            );
        }
        assert_eq!(seen.len(), 16, "a variant is missing from the list");
    }

    #[test]
    fn a_refusal_carries_what_the_caller_needs_to_act() {
        let start = EntryError::StartProcess {
            doctype: "employee".to_owned(),
            process: "entry_employee".to_owned(),
        }
        .to_app_error();
        assert_eq!(
            start.details.get("process").and_then(|v| v.as_str()),
            Some("entry_employee")
        );
        let owned = EntryError::OwnedByRequest {
            doctype: "employee".to_owned(),
            request: "req1".to_owned(),
        }
        .to_app_error();
        assert_eq!(
            owned.details.get("request").and_then(|v| v.as_str()),
            Some("req1")
        );
    }
}
