//! Stable refusals for customer consent and hardware-bound access.

/// A refused access operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeepError {
    /// A customer consent receipt is required.
    #[error("deep_access.missing_receipt: A customer consent receipt is required.")]
    MissingReceipt,
    /// The receipt approves a different request.
    #[error("deep_access.wrong_request: The receipt approves a different request.")]
    WrongRequest,
    /// The customer has not trusted this approver and key.
    #[error("deep_access.unknown_approver: The customer has not trusted this approver and key.")]
    UnknownApprover,
    /// The operator cannot approve their own access.
    #[error("deep_access.operator_approval: The operator cannot approve their own access.")]
    OperatorApproval,
    /// The customer signature is invalid.
    #[error("deep_access.bad_signature: The customer signature is invalid.")]
    BadSignature,
    /// Consent is not valid yet.
    #[error("deep_access.not_yet_valid: Consent is not valid yet.")]
    NotYetValid,
    /// Consent has expired.
    #[error("deep_access.expired_receipt: Consent has expired.")]
    ExpiredReceipt,
    /// Consent must narrow the requested scope.
    #[error("deep_access.widened_scope: Consent must narrow the requested scope.")]
    WidenedScope,
    /// The authentication method is too weak for this tier.
    #[error("deep_access.weak_method: The authentication method is too weak for this tier.")]
    WeakMethod,
    /// The consent nonce was already consumed.
    #[error("deep_access.replayed_receipt: The consent nonce was already consumed.")]
    ReplayedReceipt,
    /// A hardware-key attestation is required.
    #[error("deep_access.missing_attestation: A hardware-key attestation is required.")]
    MissingAttestation,
    /// The hardware-key attestation is invalid.
    #[error("deep_access.bad_attestation: The hardware-key attestation is invalid.")]
    BadAttestation,
    /// The hardware attestation names another operator key.
    #[error("deep_access.wrong_operator_key: The hardware attestation names another operator key.")]
    WrongOperatorKey,
    /// The access request is invalid.
    #[error("deep_access.invalid_request: The access request is invalid.")]
    InvalidRequest,
    /// The session policy is invalid.
    #[error("deep_access.invalid_policy: The session policy is invalid.")]
    InvalidPolicy,
    /// The credential is not active yet.
    #[error("deep_access.not_started: The credential is not active yet.")]
    NotStarted,
    /// The credential has expired.
    #[error("deep_access.expired: The credential has expired.")]
    Expired,
    /// The customer or platform has revoked this session.
    #[error("deep_access.revoked: The customer or platform has revoked this session.")]
    Revoked,
    /// The use counter must strictly increase.
    #[error("deep_access.replayed_use: The use counter must strictly increase.")]
    ReplayedUse,
    /// The operator did not sign this exact action and arguments.
    #[error(
        "deep_access.invalid_proof: The operator did not sign this exact action and arguments."
    )]
    InvalidProof,
    /// The action is outside the consented scope.
    #[error("deep_access.out_of_scope: The action is outside the consented scope.")]
    OutOfScope,
    /// The agent tier or hard-floor checks refused the action.
    #[error("deep_access.tier_refused: The agent tier or hard-floor checks refused the action.")]
    TierRefused,
    /// This action requires fresh, action-specific customer consent.
    #[error(
        "deep_access.step_up_required: This action requires fresh, action-specific customer consent."
    )]
    StepUpRequired,
    /// The customer audit log could not record the decision.
    #[error("deep_access.audit_unavailable: The customer audit log could not record the decision.")]
    AuditUnavailable,
    /// The authoritative nonce or revocation store is unavailable.
    #[error(
        "deep_access.state_unavailable: The authoritative nonce or revocation store is unavailable."
    )]
    StateUnavailable,
    /// An escalation must narrow to one action and cannot extend the session.
    #[error(
        "deep_access.invalid_escalation: An escalation must narrow to one action and cannot extend the session."
    )]
    InvalidEscalation,
}

impl DeepError {
    /// The stable customer-facing code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::MissingReceipt => "deep_access.missing_receipt",
            Self::WrongRequest => "deep_access.wrong_request",
            Self::UnknownApprover => "deep_access.unknown_approver",
            Self::OperatorApproval => "deep_access.operator_approval",
            Self::BadSignature => "deep_access.bad_signature",
            Self::NotYetValid => "deep_access.not_yet_valid",
            Self::ExpiredReceipt => "deep_access.expired_receipt",
            Self::WidenedScope => "deep_access.widened_scope",
            Self::WeakMethod => "deep_access.weak_method",
            Self::ReplayedReceipt => "deep_access.replayed_receipt",
            Self::MissingAttestation => "deep_access.missing_attestation",
            Self::BadAttestation => "deep_access.bad_attestation",
            Self::WrongOperatorKey => "deep_access.wrong_operator_key",
            Self::InvalidRequest => "deep_access.invalid_request",
            Self::InvalidPolicy => "deep_access.invalid_policy",
            Self::NotStarted => "deep_access.not_started",
            Self::Expired => "deep_access.expired",
            Self::Revoked => "deep_access.revoked",
            Self::ReplayedUse => "deep_access.replayed_use",
            Self::InvalidProof => "deep_access.invalid_proof",
            Self::OutOfScope => "deep_access.out_of_scope",
            Self::TierRefused => "deep_access.tier_refused",
            Self::StepUpRequired => "deep_access.step_up_required",
            Self::AuditUnavailable => "deep_access.audit_unavailable",
            Self::StateUnavailable => "deep_access.state_unavailable",
            Self::InvalidEscalation => "deep_access.invalid_escalation",
        }
    }
}
