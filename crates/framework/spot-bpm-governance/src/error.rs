//! The one error type, with stable `bpm_governance.*` codes.

use spot_app_api::{AppError, ErrorClass};

use crate::codes;

/// Everything this crate can refuse or fail with.
///
/// Every variant maps to one stable code (see [`codes`]) and one [`ErrorClass`], so a refusal
/// rides the document engine's own error envelope and the catalogue explains it. A refusal
/// never carries a field value and never names a secret.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GovernanceError {
    /// A governed record type was created outside its running request, with neither a valid case
    /// context nor the outside-process permission and a reason.
    #[error("The record type `{doctype}` is governed: it can be created only inside its request.")]
    OutsideProcessDenied {
        /// The governed record type.
        doctype: String,
    },

    /// The outside-process permission was held, but the reason was blank or too short.
    #[error(
        "An outside-process reason of at least {min_chars} characters is required to create \
         `{doctype}` outside its request."
    )]
    ReasonRequired {
        /// The governed record type.
        doctype: String,
        /// The configured floor, in characters.
        min_chars: usize,
    },

    /// A stored governance policy value could not be understood.
    #[error("The governance policy is malformed: {detail}")]
    PolicyInvalid {
        /// What was wrong, with no field value.
        detail: String,
    },

    /// A governance setting could not be read or its registry could not be built.
    #[error("The governance settings are unavailable: {detail}")]
    Settings {
        /// What was wrong, with no secret and no raw store detail.
        detail: String,
    },

    /// The governance audit entry could not be written, so the operation failed closed.
    #[error("The governance audit entry could not be written: {detail}")]
    Audit {
        /// What was wrong, with no secret and no raw store detail.
        detail: String,
    },
}

impl GovernanceError {
    /// The stable code this error carries.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutsideProcessDenied { .. } => codes::OUTSIDE_PROCESS_DENIED,
            Self::ReasonRequired { .. } => codes::REASON_REQUIRED,
            Self::PolicyInvalid { .. } => codes::POLICY_INVALID,
            Self::Settings { .. } => codes::SETTINGS,
            Self::Audit { .. } => codes::AUDIT,
        }
    }

    /// The error class the code rides, matching the catalogue entry.
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::OutsideProcessDenied { .. }
            | Self::ReasonRequired { .. }
            | Self::PolicyInvalid { .. } => ErrorClass::Validation,
            Self::Settings { .. } => ErrorClass::Internal,
            Self::Audit { .. } => ErrorClass::Unavailable,
        }
    }

    /// This error as the document engine's [`AppError`], ready to fail a `before_create` step.
    #[must_use]
    pub fn to_app_error(&self) -> AppError {
        AppError::new(self.class(), self.code(), self.to_string())
    }
}
