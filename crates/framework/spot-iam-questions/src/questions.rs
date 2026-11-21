//! The four answerable questions as permission-aware, audited queries (ADR-070 section 8,
//! rule 5).
//!
//! [`Questions`] ties the read model, the audit sink and the tenant's governance policy
//! together. Every question:
//!
//! - is **permission-checked**: asking about yourself is always allowed; asking about anyone
//!   or anything else needs the **access-governance duty**;
//! - is **audited**: a row goes to the [`QueryAuditor`] whether the asker was permitted or
//!   refused, carrying no result contents;
//! - is **paged deterministically**: the full answer is sorted on a total key and cut into
//!   pages of at most [`MAX_PAGE`](crate::page::MAX_PAGE).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::audit::{QueryAudit, QueryAuditor};
use crate::changes::changes;
use crate::engine::{ScopeQuery, build, epoch};
use crate::error::QuestionsError;
use crate::explain::{Holder, PermissionRow, PermissionSpec, holders, permissions};
use crate::model::{AccessModel, Change, DocumentRecord};
use crate::page::{Page, Paged};
use crate::visibility::{Viewer, viewers};

/// The duty a user must hold to ask about anyone other than themselves (ADR-070 section 10,
/// rule 2).
pub const ACCESS_GOVERNANCE_DUTY: &str = "Access Governance";

/// The tenant policy a query runs under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct AskPolicy {
    /// The duty that lets a user ask about others.
    pub governance_duty: String,
}

impl Default for AskPolicy {
    fn default() -> Self {
        AskPolicy {
            governance_duty: ACCESS_GOVERNANCE_DUTY.to_owned(),
        }
    }
}

/// The four questions over one read model, auditing every answer.
pub struct Questions<'a> {
    model: &'a dyn AccessModel,
    auditor: &'a dyn QueryAuditor,
    policy: AskPolicy,
}

impl<'a> Questions<'a> {
    /// The questions over `model`, audited to `auditor`, with the default policy.
    #[must_use]
    pub fn new(model: &'a dyn AccessModel, auditor: &'a dyn QueryAuditor) -> Questions<'a> {
        Questions {
            model,
            auditor,
            policy: AskPolicy::default(),
        }
    }

    /// The questions under a tenant-specific policy.
    #[must_use]
    pub fn with_policy(
        model: &'a dyn AccessModel,
        auditor: &'a dyn QueryAuditor,
        policy: AskPolicy,
    ) -> Questions<'a> {
        Questions {
            model,
            auditor,
            policy,
        }
    }

    /// Question 1 — what can this user do, and why. Each row names the assignment, role
    /// (with its inheritance path), duty, source, scope and the decision core's rule.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::RequestInvalid`] for a blank asker or user;
    /// [`QuestionsError::NotPermitted`] when the asker asks about someone else without the
    /// governance duty; [`QuestionsError::ModelInvalid`] when the read model is inconsistent.
    ///
    /// A portal user's assignment of a role that fails the portal allow-list grants nothing
    /// here (ADR-083 Barrier 3), exactly as the permission engine refuses it.
    pub fn what_can_user_do(
        &self,
        asker: &str,
        user: &str,
        at: OffsetDateTime,
        page: Page,
    ) -> Result<Paged<PermissionRow>, QuestionsError> {
        check_id(asker, "asker")?;
        check_id(user, "user")?;
        let world = build(self.model, at)?;
        self.authorise(
            &world,
            asker,
            Some(user),
            "what_can_user_do",
            user,
            epoch(at),
        )?;
        let rows = permissions(&world, self.model.assignments(), user);
        let total = rows.len();
        let answer = Paged::of(rows, page);
        self.record(asker, "what_can_user_do", user, epoch(at), total, &answer);
        Ok(answer)
    }

    /// Question 2 — who can do this, and why. `scope` describes the document being asked
    /// about; empty means "anywhere".
    ///
    /// # Errors
    ///
    /// [`QuestionsError::UnknownPermission`] for a spec that names no lifecycle action;
    /// [`QuestionsError::UnknownDoctype`] for an unknown DocType;
    /// [`QuestionsError::NotPermitted`] without the governance duty.
    pub fn who_can_do(
        &self,
        asker: &str,
        spec: &PermissionSpec,
        scope: &ScopeQuery,
        at: OffsetDateTime,
        page: Page,
    ) -> Result<Paged<Holder>, QuestionsError> {
        check_id(asker, "asker")?;
        spec.action()?;
        let world = build(self.model, at)?;
        // Authorise before anything that reveals what the model holds.
        self.authorise(
            &world,
            asker,
            None,
            "who_can_do",
            &spec.subject(),
            epoch(at),
        )?;
        if !world.knows_doctype(&spec.doctype) {
            return Err(QuestionsError::UnknownDoctype {
                doctype: spec.doctype.clone(),
                detail: "the read model has no such DocType".to_owned(),
            });
        }
        let rows = holders(&world, self.model.assignments(), spec, scope)?;
        let total = rows.len();
        let answer = Paged::of(rows, page);
        self.record(
            asker,
            "who_can_do",
            &spec.subject(),
            epoch(at),
            total,
            &answer,
        );
        Ok(answer)
    }

    /// Question 3 — who can see this document, with the field-level groups each viewer may
    /// read.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::UnknownDoctype`] for an unknown DocType;
    /// [`QuestionsError::RequestInvalid`] when the model holds no such document;
    /// [`QuestionsError::NotPermitted`] without the governance duty.
    ///
    /// A portal user holding a desk role is not listed as a viewer (ADR-083 Barrier 3).
    pub fn who_can_see(
        &self,
        asker: &str,
        doctype: &str,
        name: &str,
        at: OffsetDateTime,
        page: Page,
    ) -> Result<Paged<Viewer>, QuestionsError> {
        check_id(asker, "asker")?;
        check_id(name, "document")?;
        let world = build(self.model, at)?;
        let subject = format!("{doctype}.{name}");
        // Authorise before anything that reveals what the model holds.
        self.authorise(&world, asker, None, "who_can_see", &subject, epoch(at))?;
        if !world.knows_doctype(doctype) {
            return Err(QuestionsError::UnknownDoctype {
                doctype: doctype.to_owned(),
                detail: "the read model has no such DocType".to_owned(),
            });
        }
        let doc: DocumentRecord = self.model.document(doctype, name).cloned().ok_or_else(|| {
            QuestionsError::RequestInvalid {
                subject: subject.clone(),
                detail: "the read model holds no such document".to_owned(),
            }
        })?;
        let rows = viewers(self.model, &world, &doc)?;
        let total = rows.len();
        let answer = Paged::of(rows, page);
        self.record(asker, "who_can_see", &subject, epoch(at), total, &answer);
        Ok(answer)
    }

    /// Question 4 — what changed in access between two dates (inclusive at both ends).
    ///
    /// # Errors
    ///
    /// [`QuestionsError::RequestInvalid`] for a blank asker;
    /// [`QuestionsError::NotPermitted`] without the governance duty.
    pub fn access_changes(
        &self,
        asker: &str,
        from: i64,
        to: i64,
        at: OffsetDateTime,
        page: Page,
    ) -> Result<Paged<Change>, QuestionsError> {
        check_id(asker, "asker")?;
        let world = build(self.model, at)?;
        let subject = format!("{from}..{to}");
        self.authorise(&world, asker, None, "access_changes", &subject, epoch(at))?;
        let rows = changes(self.model.history(), from, to);
        let total = rows.len();
        let answer = Paged::of(rows, page);
        self.record(asker, "access_changes", &subject, epoch(at), total, &answer);
        Ok(answer)
    }

    /// The permission check: asking about yourself is always allowed; anything else needs the
    /// governance duty. A refusal is audited before it is returned.
    fn authorise(
        &self,
        world: &crate::engine::World,
        asker: &str,
        about: Option<&str>,
        question: &str,
        subject: &str,
        at: i64,
    ) -> Result<(), QuestionsError> {
        if about == Some(asker) {
            return Ok(());
        }
        if world.has_duty(asker, &self.policy.governance_duty) {
            return Ok(());
        }
        self.auditor
            .record(&QueryAudit::refused(asker, question, subject, at));
        Err(QuestionsError::NotPermitted {
            asker: asker.to_owned(),
            detail: format!(
                "asking about {subject} needs the `{}` duty",
                self.policy.governance_duty
            ),
        })
    }

    fn record<T>(
        &self,
        asker: &str,
        question: &str,
        subject: &str,
        at: i64,
        total: usize,
        answer: &Paged<T>,
    ) {
        self.auditor.record(&QueryAudit {
            asker: asker.to_owned(),
            question: question.to_owned(),
            subject: subject.to_owned(),
            at,
            allowed: true,
            total,
            returned: answer.items.len(),
        });
    }
}

fn check_id(value: &str, what: &str) -> Result<(), QuestionsError> {
    if value.trim().is_empty() {
        return Err(QuestionsError::RequestInvalid {
            subject: format!("({what})"),
            detail: format!("`{what}` must be non-blank"),
        });
    }
    Ok(())
}
