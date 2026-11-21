//! Every answer is audited: who asked, which question, about whom, and how large the answer
//! was — never the answer's contents (ADR-070 section 8, rule 4).
//!
//! A [`QueryAudit`] carries no secret material: the question's kind, the asker, the subject,
//! the instant, whether the asker was permitted and how many rows the answer had. The host
//! persists them through a [`QueryAuditor`]; [`MemoryAuditor`] is the reference used by the
//! tests.

use std::cell::RefCell;

use serde::{Deserialize, Serialize};

/// One audited question. Deliberately result-free: results may be large and may name people,
/// and an audit row is not where either belongs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct QueryAudit {
    /// Who asked.
    pub asker: String,
    /// Which question: `what_can_user_do`, `who_can_do`, `who_can_see` or `access_changes`.
    pub question: String,
    /// What the question was about: a user, a permission, a document or a window.
    pub subject: String,
    /// When, in integer epoch seconds; the caller stamps it.
    pub at: i64,
    /// Whether the asker was permitted to ask.
    pub allowed: bool,
    /// How many rows the full answer had (zero for a refused question).
    pub total: usize,
    /// How many rows this page carried (zero for a refused question).
    pub returned: usize,
}

impl QueryAudit {
    /// An audit row for a question that was refused before it ran.
    #[must_use]
    pub fn refused(asker: &str, question: &str, subject: &str, at: i64) -> QueryAudit {
        QueryAudit {
            asker: asker.to_owned(),
            question: question.to_owned(),
            subject: subject.to_owned(),
            at,
            allowed: false,
            total: 0,
            returned: 0,
        }
    }
}

/// Where a host records the questions it answered. Implemented by the identity service.
pub trait QueryAuditor {
    /// Records one audited question. Implementations must not fail a read because the audit
    /// sink is unavailable; they may buffer.
    fn record(&self, audit: &QueryAudit);
}

/// The reference in-memory auditor: keeps the rows so a test can assert them, and so a host
/// without a durable sink yet still audits.
#[derive(Debug, Default)]
pub struct MemoryAuditor {
    records: RefCell<Vec<QueryAudit>>,
}

impl MemoryAuditor {
    /// An empty auditor.
    #[must_use]
    pub fn new() -> MemoryAuditor {
        MemoryAuditor::default()
    }

    /// The rows recorded so far, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<QueryAudit> {
        self.records.borrow().clone()
    }

    /// The most recent row, when any.
    #[must_use]
    pub fn last(&self) -> Option<QueryAudit> {
        self.records.borrow().last().cloned()
    }
}

impl QueryAuditor for MemoryAuditor {
    fn record(&self, audit: &QueryAudit) {
        self.records.borrow_mut().push(audit.clone());
    }
}

/// An auditor that records nothing: the system path and unit tests that do not care.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoAudit;

impl QueryAuditor for NoAudit {
    fn record(&self, _audit: &QueryAudit) {}
}

/// One refused emergency request or emergency-approval decision. Deliberately free of anything
/// the refused person typed (a rejection reason, an offered escalation target, say) and of
/// role-membership detail: only the stable code says why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct EmergencyRefusal {
    /// The emergency grant (or the request id it would have had) the refusal was about.
    pub grant: String,
    /// Who asked or tried to decide.
    pub by: String,
    /// What they tried: `request`, `approve` or `reject`.
    pub attempted: String,
    /// When, in integer epoch seconds; the caller stamps it.
    pub at: i64,
    /// Why it was refused: `iam_questions.escalation_target_invalid` for a request whose
    /// escalation target is unusable, `iam_governance.approver_is_grantee`,
    /// `iam_governance.not_the_approver`, `iam_governance.approval_not_pending`, or
    /// `iam_questions.emergency_state` for a grant in the wrong phase.
    pub code: String,
}

/// Where a host records the emergency decisions it refused. Implemented by the identity service.
pub trait EmergencyAuditor {
    /// Records one refusal. Implementations must not drop it; they may buffer.
    fn record(&self, refusal: &EmergencyRefusal);
}

/// The reference in-memory emergency auditor: keeps the rows so a test can assert them.
#[derive(Debug, Default)]
pub struct MemoryEmergencyAuditor {
    records: RefCell<Vec<EmergencyRefusal>>,
}

impl MemoryEmergencyAuditor {
    /// An empty auditor.
    #[must_use]
    pub fn new() -> MemoryEmergencyAuditor {
        MemoryEmergencyAuditor::default()
    }

    /// The rows recorded so far, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<EmergencyRefusal> {
        self.records.borrow().clone()
    }

    /// The most recent row, when any.
    #[must_use]
    pub fn last(&self) -> Option<EmergencyRefusal> {
        self.records.borrow().last().cloned()
    }
}

impl EmergencyAuditor for MemoryEmergencyAuditor {
    fn record(&self, refusal: &EmergencyRefusal) {
        self.records.borrow_mut().push(refusal.clone());
    }
}
