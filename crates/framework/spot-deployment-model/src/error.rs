//! The deployment model's typed errors, each with a stable `deployment_model.*` code.
//!
//! The codes are catalogued in `crates/framework/spot-errors/catalogue/deployment_model.toml`.

use std::fmt;

/// The stable error codes this crate raises.
///
/// Every code has an entry in the error catalogue.
pub mod codes {
    /// A fact, event or model field is empty, too long or otherwise malformed.
    pub const INVALID: &str = "deployment_model.invalid";
    /// The ground truth does not carry a fact the model requires.
    pub const FACTS_INCOMPLETE: &str = "deployment_model.facts_incomplete";
    /// Two fact sources give the same fact different values.
    pub const FACTS_CONFLICT: &str = "deployment_model.facts_conflict";
    /// A fact source could not produce its facts.
    pub const FACTS_UNREADABLE: &str = "deployment_model.facts_unreadable";
    /// A deployment event cannot be applied to the model.
    pub const EVENT_INVALID: &str = "deployment_model.event_invalid";
    /// The model could not be serialised for its digest.
    pub const SERIALISE: &str = "deployment_model.serialise";
}

/// Everything that can go wrong while building, refreshing or updating a deployment model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeploymentError {
    /// A field, fact or event value is malformed.
    Invalid {
        /// Which field, fact or value was refused.
        field: String,
        /// Why it was refused.
        reason: String,
    },
    /// The ground truth lacks a fact the model requires.
    FactsIncomplete {
        /// The missing fact's name.
        field: String,
    },
    /// Two sources disagree about one fact.
    FactsConflict {
        /// The fact they disagree about.
        field: String,
    },
    /// A source could not be read.
    FactsUnreadable {
        /// The source's stable name.
        source: String,
        /// Why it could not be read.
        reason: String,
    },
    /// An event cannot be applied to this model.
    EventInvalid {
        /// Why it was refused.
        reason: String,
    },
    /// The model could not be serialised for its digest.
    Serialise {
        /// Why serialisation failed.
        reason: String,
    },
}

impl DeploymentError {
    /// The stable code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => codes::INVALID,
            Self::FactsIncomplete { .. } => codes::FACTS_INCOMPLETE,
            Self::FactsConflict { .. } => codes::FACTS_CONFLICT,
            Self::FactsUnreadable { .. } => codes::FACTS_UNREADABLE,
            Self::EventInvalid { .. } => codes::EVENT_INVALID,
            Self::Serialise { .. } => codes::SERIALISE,
        }
    }

    /// A malformed value, naming the field but never a secret value.
    #[must_use]
    pub fn invalid(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Invalid {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for DeploymentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { field, reason } => {
                write!(f, "{field} is not valid: {reason}")
            }
            Self::FactsIncomplete { field } => {
                write!(f, "the ground truth carries no {field}")
            }
            Self::FactsConflict { field } => {
                write!(f, "two fact sources disagree about {field}")
            }
            Self::FactsUnreadable { source, reason } => {
                write!(f, "source {source} could not be read: {reason}")
            }
            Self::EventInvalid { reason } => {
                write!(f, "the deployment event is invalid: {reason}")
            }
            Self::Serialise { reason } => {
                write!(f, "the deployment model could not be serialised: {reason}")
            }
        }
    }
}

impl std::error::Error for DeploymentError {}

/// A value must be non-empty and no longer than this many bytes.
pub(crate) const MAX_TEXT_BYTES: usize = 256;

/// Accepts a non-empty, bounded, control-character-free value.
pub(crate) fn check_text(field: &str, value: &str, max: usize) -> Result<(), DeploymentError> {
    if value.trim().is_empty() {
        return Err(DeploymentError::invalid(field, "the value is empty"));
    }
    if value.len() > max {
        return Err(DeploymentError::invalid(
            field,
            format!("the value is longer than {max} bytes"),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(DeploymentError::invalid(
            field,
            "the value contains a control character",
        ));
    }
    Ok(())
}
