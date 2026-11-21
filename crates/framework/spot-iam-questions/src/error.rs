//! The one error type, with stable, machine-readable `iam_questions.*` codes.
//!
//! Codes are stable: for logs, metrics and API responses use [`QuestionsError::code`], never
//! the message text. Every error names the subject (a question, a user, a document, a grant,
//! a review) it is about, so a caller fixing a refused call sees which subject each problem
//! belongs to.

/// Something failed while answering a question or running an emergency grant.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum QuestionsError {
    /// A question, a permission spec, a document reference, a reason or an id is malformed:
    /// a blank value where the model needs one.
    #[error("{subject}: {detail}")]
    RequestInvalid {
        /// What the problem is about.
        subject: String,
        /// What is wrong and what the model needs.
        detail: String,
    },

    /// An emergency request's escalation target is not usable: empty, blank, carrying a
    /// control character, or longer than the target bound. The request is not
    /// created, and the value the requester offered is never echoed.
    #[error("{subject}: {detail}")]
    EscalationTargetInvalid {
        /// The request id the refused request would have had.
        subject: String,
        /// The rule the target has to satisfy.
        detail: String,
    },

    /// The asker may not ask about another user: only a holder of the access-governance duty
    /// may ask about someone else (ADR-070 section 8, rule 5).
    #[error("{asker}: {detail}")]
    NotPermitted {
        /// The asker who was refused.
        asker: String,
        /// Why the asker may not ask.
        detail: String,
    },

    /// The question names a DocType the read model does not know.
    #[error("{doctype}: {detail}")]
    UnknownDoctype {
        /// The unknown DocType.
        doctype: String,
        /// What was looked for.
        detail: String,
    },

    /// The question names a permission (an action) the model does not recognise.
    #[error("{permission}: {detail}")]
    UnknownPermission {
        /// The text the caller passed.
        permission: String,
        /// What the parser accepts.
        detail: String,
    },

    /// The read model is internally inconsistent: a broken registry, an invalid assignment,
    /// a grant on an unknown field, an undeclared protection group.
    #[error("{subject}: {detail}")]
    ModelInvalid {
        /// What the model objected to.
        subject: String,
        /// The problem, in the model's own words.
        detail: String,
    },

    /// A page request is out of range: a zero or oversized limit.
    #[error("{subject}: {detail}")]
    PageInvalid {
        /// What the problem is about.
        subject: String,
        /// What the model accepts.
        detail: String,
    },

    /// An emergency grant was asked to move to a state its current state does not allow.
    #[error("{grant}: {detail}")]
    EmergencyState {
        /// The grant.
        grant: String,
        /// Its current state and what was refused.
        detail: String,
    },

    /// An emergency grant asks for a duration its policy does not allow, or a non-positive
    /// one.
    #[error("{grant}: {detail}")]
    EmergencyDuration {
        /// The grant.
        grant: String,
        /// The requested and maximum durations.
        detail: String,
    },

    /// The person asked to review an emergency session is the session's own user: an
    /// emergency session is never reviewed by its own user (the firefighter rule).
    #[error("{review}: {detail}")]
    ReviewSelf {
        /// The review.
        review: String,
        /// The user who is both session user and reviewer.
        detail: String,
    },

    /// The person asked to record a review decision is not the review's controller (or its
    /// required second controller).
    #[error("{review}: {detail}")]
    ReviewWrongPerson {
        /// The review.
        review: String,
        /// Who was asked and who may decide.
        detail: String,
    },

    /// The session recorder refused a frame, or the action is not tagged with the grant.
    #[error("{subject}: {detail}")]
    Recording {
        /// What was being recorded.
        subject: String,
        /// Why it was refused.
        detail: String,
    },

    /// The BPM launcher refused the emergency approval Process.
    #[error("{request}: {detail}")]
    Approval {
        /// The grant request.
        request: String,
        /// What the launcher or process builder objected to.
        detail: String,
    },
}

/// The stable machine-readable codes, with their meanings.
impl QuestionsError {
    /// The stable machine-readable code of this error.
    ///
    /// The codes, with their meanings:
    ///
    /// - `iam_questions.request_invalid` — a blank id, subject, reason or scope value;
    /// - `iam_questions.escalation_target_invalid` — an emergency request's escalation target
    ///   is empty, blank, carries a control character or is too long;
    /// - `iam_questions.not_permitted` — the asker may not ask about another user;
    /// - `iam_questions.unknown_doctype` — the question names an unknown DocType;
    /// - `iam_questions.unknown_permission` — the question names an action the model does not
    ///   know;
    /// - `iam_questions.model_invalid` — the read model is internally inconsistent;
    /// - `iam_questions.page_invalid` — a page request is out of range;
    /// - `iam_questions.emergency_state` — the requested transition is not allowed now;
    /// - `iam_questions.emergency_duration` — the requested duration is not allowed;
    /// - `iam_questions.review_self` — a session is never reviewed by its own user;
    /// - `iam_questions.review_wrong_person` — the decider is not the review's controller;
    /// - `iam_questions.recording` — the session recorder refused a frame;
    /// - `iam_questions.approval` — the BPM launcher refused the approval Process.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RequestInvalid { .. } => REQUEST_INVALID,
            Self::EscalationTargetInvalid { .. } => ESCALATION_TARGET_INVALID,
            Self::NotPermitted { .. } => NOT_PERMITTED,
            Self::UnknownDoctype { .. } => UNKNOWN_DOCTYPE,
            Self::UnknownPermission { .. } => UNKNOWN_PERMISSION,
            Self::ModelInvalid { .. } => MODEL_INVALID,
            Self::PageInvalid { .. } => PAGE_INVALID,
            Self::EmergencyState { .. } => EMERGENCY_STATE,
            Self::EmergencyDuration { .. } => EMERGENCY_DURATION,
            Self::ReviewSelf { .. } => REVIEW_SELF,
            Self::ReviewWrongPerson { .. } => REVIEW_WRONG_PERSON,
            Self::Recording { .. } => RECORDING,
            Self::Approval { .. } => APPROVAL,
        }
    }
}

/// A blank id, subject, reason or scope value.
pub const REQUEST_INVALID: &str = "iam_questions.request_invalid";
/// An emergency request's escalation target is not usable.
pub const ESCALATION_TARGET_INVALID: &str = "iam_questions.escalation_target_invalid";
/// The asker may not ask about another user.
pub const NOT_PERMITTED: &str = "iam_questions.not_permitted";
/// The question names an unknown DocType.
pub const UNKNOWN_DOCTYPE: &str = "iam_questions.unknown_doctype";
/// The question names an action the model does not know.
pub const UNKNOWN_PERMISSION: &str = "iam_questions.unknown_permission";
/// The read model is internally inconsistent.
pub const MODEL_INVALID: &str = "iam_questions.model_invalid";
/// A page request is out of range.
pub const PAGE_INVALID: &str = "iam_questions.page_invalid";
/// The requested emergency transition is not allowed now.
pub const EMERGENCY_STATE: &str = "iam_questions.emergency_state";
/// The requested emergency duration is not allowed.
pub const EMERGENCY_DURATION: &str = "iam_questions.emergency_duration";
/// A session is never reviewed by its own user.
pub const REVIEW_SELF: &str = "iam_questions.review_self";
/// The decider is not the review's controller.
pub const REVIEW_WRONG_PERSON: &str = "iam_questions.review_wrong_person";
/// The session recorder refused a frame.
pub const RECORDING: &str = "iam_questions.recording";
/// The BPM launcher refused the approval Process.
pub const APPROVAL: &str = "iam_questions.approval";

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample() -> Vec<(QuestionsError, &'static str)> {
        vec![
            (
                QuestionsError::RequestInvalid {
                    subject: "(question)".to_owned(),
                    detail: "needs a user".to_owned(),
                },
                REQUEST_INVALID,
            ),
            (
                QuestionsError::EscalationTargetInvalid {
                    subject: "eg-1".to_owned(),
                    detail: "the escalation target is not usable".to_owned(),
                },
                ESCALATION_TARGET_INVALID,
            ),
            (
                QuestionsError::NotPermitted {
                    asker: "ana".to_owned(),
                    detail: "needs the access-governance duty".to_owned(),
                },
                NOT_PERMITTED,
            ),
            (
                QuestionsError::UnknownDoctype {
                    doctype: "invoice".to_owned(),
                    detail: "not in the model".to_owned(),
                },
                UNKNOWN_DOCTYPE,
            ),
            (
                QuestionsError::UnknownPermission {
                    permission: "invoice.frobnicate".to_owned(),
                    detail: "not an action".to_owned(),
                },
                UNKNOWN_PERMISSION,
            ),
            (
                QuestionsError::ModelInvalid {
                    subject: "registry".to_owned(),
                    detail: "a cycle".to_owned(),
                },
                MODEL_INVALID,
            ),
            (
                QuestionsError::PageInvalid {
                    subject: "(page)".to_owned(),
                    detail: "limit is 0".to_owned(),
                },
                PAGE_INVALID,
            ),
            (
                QuestionsError::EmergencyState {
                    grant: "eg-1".to_owned(),
                    detail: "not active".to_owned(),
                },
                EMERGENCY_STATE,
            ),
            (
                QuestionsError::EmergencyDuration {
                    grant: "eg-1".to_owned(),
                    detail: "too long".to_owned(),
                },
                EMERGENCY_DURATION,
            ),
            (
                QuestionsError::ReviewSelf {
                    review: "rv-1".to_owned(),
                    detail: "ben".to_owned(),
                },
                REVIEW_SELF,
            ),
            (
                QuestionsError::ReviewWrongPerson {
                    review: "rv-1".to_owned(),
                    detail: "cara".to_owned(),
                },
                REVIEW_WRONG_PERSON,
            ),
            (
                QuestionsError::Recording {
                    subject: "eg-1".to_owned(),
                    detail: "the recorder refused".to_owned(),
                },
                RECORDING,
            ),
            (
                QuestionsError::Approval {
                    request: "eg-1".to_owned(),
                    detail: "the launcher refused".to_owned(),
                },
                APPROVAL,
            ),
        ]
    }

    #[test]
    fn codes_are_stable_and_cover_every_variant() {
        for (error, code) in sample() {
            assert_eq!(error.code(), code, "code drifted for {error:?}");
        }
    }
}
