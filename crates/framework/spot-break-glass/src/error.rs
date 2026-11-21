//! The one error type of break-glass access, with stable `break_glass.*` codes.
//!
//! Every refusal the crate can raise is a [`BreakGlassError`]; [`BreakGlassError::code`]
//! is the stable machine-readable code, and the `Display` text is a plain sentence. The
//! code names never carry a secret, a ticket body or a shell command: a refusal that a
//! customer reads says what was wrong, never what was typed.

/// A refused break-glass operation.
///
/// The codes are stable (`break_glass.*`); messages are for people.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BreakGlassError {
    /// The tenant has not opted in to break-glass support access.
    #[error("break_glass.not_opted_in: The tenant has not opted in to break-glass access.")]
    NotOptedIn,

    /// The request's reason is blank.
    #[error("break_glass.reason_required: A break-glass request needs a reason.")]
    ReasonRequired,

    /// The request's ticket id is blank.
    #[error("break_glass.ticket_required: A break-glass request needs a support ticket id.")]
    TicketRequired,

    /// The requester's identity is blank.
    #[error("break_glass.requester_blank: A break-glass request needs a named requester.")]
    RequesterBlank,

    /// The window ends before it starts, or starts before the request was made.
    #[error("break_glass.window_invalid: The break-glass window is not a valid half-open range.")]
    WindowInvalid,

    /// The window is longer than the policy (or the tenant setting) allows.
    #[error(
        "break_glass.duration_over_policy: The window would run {asked} s, over the cap of {max} s."
    )]
    DurationOverPolicy {
        /// The asked duration, in seconds.
        asked: i64,
        /// The effective cap, in seconds.
        max: i64,
    },

    /// The transition does not apply in the session's current state.
    #[error("break_glass.wrong_state: The session is in the wrong state for that: {0}")]
    WrongState(&'static str),

    /// An approver's identity is blank.
    #[error("break_glass.approver_blank: An approver needs a named identity.")]
    ApproverBlank,

    /// An approval came from a principal who is not a vendor-side person.
    #[error("break_glass.approver_not_vendor: The two-person approval is vendor-side only.")]
    ApproverNotVendor,

    /// The requester tried to approve their own request.
    #[error(
        "break_glass.requester_cannot_approve: The requester cannot approve their own request."
    )]
    RequesterCannotApprove,

    /// The same approver approved twice.
    #[error("break_glass.duplicate_approver: An approver may approve a request only once.")]
    DuplicateApprover,

    /// Fewer than the required number of distinct approvals are present.
    #[error("break_glass.not_enough_approvers: {have} of {need} required approvals are present.")]
    NotEnoughApprovers {
        /// How many distinct approvals are present.
        have: usize,
        /// How many the policy requires.
        need: usize,
    },

    /// The tenant's policy requires customer consent and none was recorded.
    #[error(
        "break_glass.customer_consent_required: The tenant's policy requires customer consent."
    )]
    CustomerConsentRequired,

    /// The recorded consent does not match the session's tenant or window.
    #[error("break_glass.consent_mismatch: The customer consent does not cover this session.")]
    ConsentMismatch,

    /// The composed grant was refused (deep-access or support-session).
    #[error("break_glass.grant_refused: The composed grant refused the session: {0}")]
    GrantRefused(&'static str),

    /// The composed grant covers another tenant or a wider window than the request.
    #[error("break_glass.grant_mismatch: The composed grant does not match the request.")]
    GrantMismatch,

    /// The session id is not known.
    #[error("break_glass.unknown_session: No break-glass session has that id.")]
    UnknownSession,

    /// The session may not act yet.
    #[error("break_glass.not_started: The break-glass session has not started yet.")]
    NotStarted,

    /// The session is over.
    #[error("break_glass.expired: The break-glass session has expired.")]
    Expired,

    /// The session was revoked.
    #[error("break_glass.revoked: The break-glass session was revoked.")]
    Revoked,

    /// The recording file could not be read or written.
    #[error("break_glass.recording_io: The recording could not be read or written.")]
    RecordingIo,

    /// The recording's hash chain does not verify: it was altered.
    #[error(
        "break_glass.recording_tampered: The recording was altered: frame {frame} does not verify."
    )]
    RecordingTampered {
        /// The sequence number of the first frame that failed.
        frame: u64,
    },

    /// The recording file does not exist.
    #[error("break_glass.recording_not_found: No recording exists for that session.")]
    RecordingNotFound,

    /// A frame is malformed: the wrong length, an unknown kind or a bad header.
    #[error("break_glass.frame_invalid: The recording carries a malformed frame.")]
    FrameInvalid,

    /// The manifest is malformed or does not match the recording.
    #[error(
        "break_glass.manifest_invalid: The recording manifest is malformed or disagrees with the recording."
    )]
    ManifestInvalid,

    /// The recording reached its size cap; the truncation marker is written.
    #[error("break_glass.recording_full: The recording reached its size cap and is truncated.")]
    RecordingFull,

    /// A vendor-side caller tried to delete a customer recording.
    #[error("break_glass.vendor_delete_refused: A vendor cannot delete a customer recording.")]
    VendorDeleteRefused,

    /// Deletion was asked of an append-only recording archive.
    #[error("break_glass.append_only: Recordings are append-only and are never deleted in place.")]
    AppendOnly,

    /// No interactive terminal is available.
    #[error("break_glass.pty_unavailable: No interactive terminal is available.")]
    PtyUnavailable,

    /// The customer audit log could not record the transition.
    #[error(
        "break_glass.audit_unavailable: The customer audit log could not record the transition."
    )]
    AuditUnavailable,

    /// Time went backwards between two observations of the monotonic clock.
    #[error(
        "break_glass.clock_went_backwards: The monotonic clock went backwards; the session fails closed."
    )]
    ClockWentBackwards,
}

impl BreakGlassError {
    /// The stable customer-facing code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NotOptedIn => "break_glass.not_opted_in",
            Self::ReasonRequired => "break_glass.reason_required",
            Self::TicketRequired => "break_glass.ticket_required",
            Self::RequesterBlank => "break_glass.requester_blank",
            Self::WindowInvalid => "break_glass.window_invalid",
            Self::DurationOverPolicy { .. } => "break_glass.duration_over_policy",
            Self::WrongState(_) => "break_glass.wrong_state",
            Self::ApproverBlank => "break_glass.approver_blank",
            Self::ApproverNotVendor => "break_glass.approver_not_vendor",
            Self::RequesterCannotApprove => "break_glass.requester_cannot_approve",
            Self::DuplicateApprover => "break_glass.duplicate_approver",
            Self::NotEnoughApprovers { .. } => "break_glass.not_enough_approvers",
            Self::CustomerConsentRequired => "break_glass.customer_consent_required",
            Self::ConsentMismatch => "break_glass.consent_mismatch",
            Self::GrantRefused(_) => "break_glass.grant_refused",
            Self::GrantMismatch => "break_glass.grant_mismatch",
            Self::UnknownSession => "break_glass.unknown_session",
            Self::NotStarted => "break_glass.not_started",
            Self::Expired => "break_glass.expired",
            Self::Revoked => "break_glass.revoked",
            Self::RecordingIo => "break_glass.recording_io",
            Self::RecordingTampered { .. } => "break_glass.recording_tampered",
            Self::RecordingNotFound => "break_glass.recording_not_found",
            Self::FrameInvalid => "break_glass.frame_invalid",
            Self::ManifestInvalid => "break_glass.manifest_invalid",
            Self::RecordingFull => "break_glass.recording_full",
            Self::VendorDeleteRefused => "break_glass.vendor_delete_refused",
            Self::AppendOnly => "break_glass.append_only",
            Self::PtyUnavailable => "break_glass.pty_unavailable",
            Self::AuditUnavailable => "break_glass.audit_unavailable",
            Self::ClockWentBackwards => "break_glass.clock_went_backwards",
        }
    }
}

/// The stable code of an allowed break-glass decision, for the customer view.
pub const ALLOW: &str = "break_glass.allow";
