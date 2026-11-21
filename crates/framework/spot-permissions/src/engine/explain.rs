//! The support views: `explain`, `explain_field` and the stable machine form, as
//! `impl PermissionEngine` split out of the document engine.
//!
//! The support form is for people; the machine form (`explain_machine`) is the one that is
//! parsed, and it is stable across releases.

use super::{Action, DocFacts, PermissionEngine};
use crate::model::{FieldFacts, Principal};

impl PermissionEngine {
    /// The support view: every reason in one human block, each naming its assignment.
    #[must_use]
    pub fn explain(&self, principal: &Principal, action: Action, doc: &DocFacts) -> String {
        let d = self.can(principal, action, doc);
        let mut out = format!(
            "{} {} {} by {}: {}\n",
            action.as_str(),
            doc.doctype,
            doc.id.as_deref().unwrap_or("(new)"),
            if principal.is_system {
                "system"
            } else {
                &principal.user
            },
            if d.allowed { "ALLOWED" } else { "DENIED" }
        );
        for r in &d.reasons {
            out.push_str(&format!("  - {}: {}\n", r.kind(), r.sentence()));
        }
        out
    }

    /// The support view of one field decision.
    #[must_use]
    pub fn explain_field(
        &self,
        principal: &Principal,
        doctype: &str,
        field: &FieldFacts,
        doc: &DocFacts,
    ) -> String {
        let d = self.field_access(principal, doctype, field, doc);
        let mut out = format!(
            "{doctype}.{} by {}: level {:?}, create {}, update {}, report {}\n",
            field.path,
            if principal.is_system {
                "system"
            } else {
                &principal.user
            },
            d.level,
            d.can_create,
            d.can_update,
            d.can_report
        );
        for r in &d.reasons {
            out.push_str(&format!("  - {}: {}\n", r.kind(), r.sentence()));
        }
        out
    }

    /// The machine form: one line per decision, the reason codes joined by newlines, stable
    /// across releases (parse this, never the support view).
    #[must_use]
    pub fn explain_machine(&self, principal: &Principal, action: Action, doc: &DocFacts) -> String {
        let d = self.can(principal, action, doc);
        format!(
            "{} {} {}\n{}",
            action.as_str(),
            doc.doctype,
            if d.allowed { "allow" } else { "deny" },
            d.codes().join("\n")
        )
    }
}
