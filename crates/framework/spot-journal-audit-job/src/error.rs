//! Typed errors with stable `journal_audit_job.*` codes.
//!
//! An error here is the job's own refusal or a provider that could not answer. It is never an
//! audit finding: a mismatch or a failed rebuild is a result row and a condition. No message
//! carries record content, only the name of a provider, a side or a stable code.

/// Why part of the scheduled audit could not be done.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditJobError {
    /// A provider (the tenant directory, the audit inputs or the result sink) could not answer.
    #[error("the {provider} provider could not answer")]
    ProviderFailed {
        /// Which provider: `directory`, `inputs` or `sink`.
        provider: &'static str,
    },
    /// The inputs opened for a tenant belong to another tenant than the plan.
    #[error("the {side} belongs to another tenant than the plan")]
    TenantMismatch {
        /// What disagreed: `inputs`.
        side: &'static str,
    },
    /// The conditions framework refused a raise or a clear.
    #[error("the conditions framework refused the update: {code}")]
    ConditionFailed {
        /// The framework's stable code.
        code: &'static str,
    },
    /// A tenant's audit panicked; the panic was contained at the tenant boundary.
    #[error("the audit of one tenant panicked")]
    TenantPanicked,
    /// The blocking task that ran a pass did not finish (it panicked or was cancelled).
    #[error("the audit pass was aborted before it finished")]
    PassAborted,
}

impl AuditJobError {
    /// The stable catalogue code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ProviderFailed { .. } => "journal_audit_job.provider_failed",
            Self::TenantMismatch { .. } => "journal_audit_job.tenant_mismatch",
            Self::ConditionFailed { .. } => "journal_audit_job.condition_failed",
            Self::TenantPanicked => "journal_audit_job.tenant_panicked",
            Self::PassAborted => "journal_audit_job.pass_aborted",
        }
    }
}
