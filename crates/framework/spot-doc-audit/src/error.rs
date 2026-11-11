//! The one error type of the crate and its stable `docaudit.*` codes.
//!
//! Every code here has an entry in `crates/framework/spot-errors/catalogue/docaudit.toml`. A
//! message names no value: only the code, the operation and (for a chain failure) the lane and
//! sequence number.

use spot_app_api::{AppError, ErrorClass};

/// The audit event could not be written to the outbox; the operation fails closed.
pub const OUTBOX_WRITE: &str = "docaudit.outbox_write";

/// A business DocType was marked audit off.
pub const AUDIT_OFF: &str = "docaudit.audit_off";

/// The lane count or epoch of a site checkpoint does not match the configured lanes.
pub const LANE_EPOCH: &str = "docaudit.lane_epoch";

/// An event, a diff or a checkpoint could not be encoded or decoded.
pub const ENCODING: &str = "docaudit.encoding";

/// A lane append or a chain verification failed.
pub const CHAIN: &str = "docaudit.chain";

/// The field guard has no protection facts for the DocType.
pub const UNKNOWN_DOCTYPE: &str = "docaudit.unknown_doctype";

/// The field guard refused while building or reading a diff.
pub const GUARD: &str = "docaudit.guard";

/// A host registered both the audit participant and an audit step for the same DocType.
pub const DOUBLE_REGISTRATION: &str = "docaudit.double_registration";

/// A refused operation could not be recorded in the audit (logged once, counted).
pub const DENIAL_NOT_RECORDED: &str = "docaudit.denial_not_recorded";

/// Every error this crate returns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DocAuditError {
    /// The event could not be written to the outbox in the business transaction.
    #[error("the document audit event could not be written to the outbox: {0}")]
    OutboxWrite(String),
    /// A business DocType was marked audit off.
    #[error("the DocType `{0}` may not opt out of the document audit")]
    AuditOff(String),
    /// The lane count or epoch of a site checkpoint does not match the configuration.
    #[error(
        "the site checkpoint is epoch {checkpoint} with {checkpoint_n} lanes, not epoch {epoch} with {n}"
    )]
    LaneEpoch {
        /// The configured epoch.
        epoch: u32,
        /// The configured lane count.
        n: u32,
        /// The checkpoint's epoch.
        checkpoint: u32,
        /// The checkpoint's lane count.
        checkpoint_n: u32,
    },
    /// An event, a diff or a checkpoint could not be encoded or decoded.
    #[error("the document audit encoding failed: {0}")]
    Encoding(String),
    /// A lane append or a chain verification failed.
    #[error("the document audit chain failed: {0}")]
    Chain(String),
    /// The field guard has no protection facts for the DocType.
    #[error("the field guard does not know the DocType `{0}`")]
    UnknownDoctype(String),
    /// The field guard refused while building or reading a diff.
    #[error("the field guard refused the document audit diff: {0}")]
    Guard(String),
    /// The audit participant and an audit step are both registered, so every write would be
    /// audited twice. Names the app and DocType of the step (never a value).
    #[error(
        "the audit participant and the audit step of app `{app}` for DocType `{doctype}` are both registered"
    )]
    DoubleRegistration {
        /// The app that registered the audit step (empty when the duplicate is a second
        /// participant).
        app: String,
        /// The DocType the step audits (empty when the duplicate is a second participant).
        doctype: String,
    },
    /// A refused operation could not be recorded.
    #[error("a refused operation could not be recorded in the document audit: {0}")]
    DenialNotRecorded(String),
}

impl DocAuditError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutboxWrite(_) => OUTBOX_WRITE,
            Self::AuditOff(_) => AUDIT_OFF,
            Self::LaneEpoch { .. } => LANE_EPOCH,
            Self::Encoding(_) => ENCODING,
            Self::Chain(_) => CHAIN,
            Self::UnknownDoctype(_) => UNKNOWN_DOCTYPE,
            Self::Guard(_) => GUARD,
            Self::DoubleRegistration { .. } => DOUBLE_REGISTRATION,
            Self::DenialNotRecorded(_) => DENIAL_NOT_RECORDED,
        }
    }

    /// The error class the code rides on.
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::OutboxWrite(_) | Self::DenialNotRecorded(_) => ErrorClass::Unavailable,
            Self::AuditOff(_) | Self::LaneEpoch { .. } | Self::DoubleRegistration { .. } => {
                ErrorClass::Validation
            }
            Self::UnknownDoctype(_) => ErrorClass::UnknownDocType,
            Self::Encoding(_) | Self::Chain(_) | Self::Guard(_) => ErrorClass::Internal,
        }
    }

    /// The app API error, for a controller step that must fail the operation.
    #[must_use]
    pub fn to_app_error(&self) -> AppError {
        AppError::new(self.class(), self.code(), self.to_string())
    }
}

impl From<spot_audit_chain::AuditError> for DocAuditError {
    fn from(error: spot_audit_chain::AuditError) -> Self {
        Self::Chain(format!("{}: {error}", error.code()))
    }
}
