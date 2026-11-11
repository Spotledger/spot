//! The link and value restrictions: `check_links` and `allowed_values`, the scope half of
//! ADR-070 section 3, as `impl PermissionEngine` split out of the document engine.

use std::collections::BTreeSet;

use spot_events_contract::lifecycle::Operation;

use super::{Action, Allowed, DocFacts, PermissionEngine, Right, Violation};
use crate::engine::rules::{sorted_scope, value_text};
use crate::model::{AssignmentContext, Principal, TENANT_ADMINISTRATOR};
use crate::snapshot::DocumentData;

impl PermissionEngine {
    /// Checks every link (and dynamic link) value of the document against the scope of the
    /// assignment that would otherwise grant the action's right (child-table rows included,
    /// their paths `table[idx].field`, idx from 1).
    ///
    /// When any assignment that holds the right covers the document there is no violation;
    /// otherwise the violations of the granting assignments come back, in definition order
    /// then row order. An action no assignment grants has none: `can` refuses it.
    #[must_use]
    pub fn check_links(
        &self,
        principal: &Principal,
        action: Action,
        facts: &DocFacts,
        data: &DocumentData,
    ) -> Vec<Violation> {
        if principal.is_system {
            return vec![];
        }
        let doctype = facts.doctype.as_str();
        let needed: &[Right] = match action {
            Action::Correct => &[Right::Cancel, Right::Create],
            Action::Op(Operation::Create) => &[Right::Create],
            Action::Op(Operation::Update | Operation::Rename) => &[Right::Write],
            Action::Op(Operation::Post) => &[Right::Submit],
            Action::Op(Operation::Cancel) => &[Right::Cancel],
            Action::Op(Operation::Delete) => &[Right::Delete],
            Action::Op(Operation::UpdatePosted) => &[Right::PostUpdate],
            Action::Read => &[Right::Read],
        };
        let owner = facts.owned_by(principal);
        let mut granting: Vec<&AssignmentContext> = principal
            .assignments
            .iter()
            .filter(|a| {
                needed
                    .iter()
                    .all(|r| self.assignment_holds(a, doctype, *r, owner))
            })
            .collect();
        granting.sort_by(|a, b| a.id.cmp(&b.id));
        let mut out: Vec<Violation> = Vec::new();
        for a in granting {
            let violations = self.assignment_violations(a, doctype, data);
            if violations.is_empty() {
                return vec![];
            }
            for v in violations {
                if !out.contains(&v) {
                    out.push(v);
                }
            }
        }
        out
    }

    fn assignment_holds(
        &self,
        a: &AssignmentContext,
        doctype: &str,
        right: Right,
        owner: bool,
    ) -> bool {
        a.roles.iter().any(|role| {
            role.as_str() == TENANT_ADMINISTRATOR
                || self
                    .grants_of(doctype, role, right)
                    .any(|g| !g.if_owner || owner)
        })
    }

    fn assignment_violations(
        &self,
        a: &AssignmentContext,
        doctype: &str,
        data: &DocumentData,
    ) -> Vec<Violation> {
        let Some(facts) = self.snapshot.facts(doctype) else {
            return vec![];
        };
        let mut out = Vec::new();
        for up in sorted_scope(a) {
            if up.doctype.as_deref().is_some_and(|d| d != doctype) {
                continue;
            }
            for lf in &facts.links {
                let Some(v) = data.fields.get(&lf.name) else {
                    continue;
                };
                let target = match lf.target.clone() {
                    Some(t) => t,
                    None => data
                        .dynamic_targets
                        .get(&lf.name)
                        .cloned()
                        .unwrap_or_default(),
                };
                let path = format!("{doctype}.{}", lf.name);
                if up.restricts(doctype, &path, &target) {
                    let text = value_text(v);
                    if !up.allowed.iter().any(|x| x == &text) {
                        out.push(Violation {
                            path: lf.name.clone(),
                            value: text,
                        });
                    }
                }
            }
            for (table, rows) in &data.tables {
                let Some(child) = self.snapshot.facts(table) else {
                    continue;
                };
                for (i, row) in rows.iter().enumerate() {
                    for lf in &child.links {
                        let Some(v) = row.get(&lf.name) else {
                            continue;
                        };
                        let target = match lf.target.clone() {
                            Some(t) => t,
                            None => data
                                .dynamic_targets
                                .get(&format!("{table}.{}", lf.name))
                                .cloned()
                                .unwrap_or_default(),
                        };
                        let path = format!("{table}.{}", lf.name);
                        if up.restricts(doctype, &path, &target) {
                            let text = value_text(v);
                            if !up.allowed.iter().any(|x| x == &text) {
                                out.push(Violation {
                                    path: format!("{table}[{}].{}", i + 1, lf.name),
                                    value: text,
                                });
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// What the principal may enter in one restricted field: [`Allowed::All`] when some
    /// assignment that can read the DocType is not restricted on it, otherwise
    /// [`Allowed::Only`] with the union of what its assignments allow. This is what the query
    /// compiler (WP-PLT-04) turns into a list or lookup filter.
    #[must_use]
    pub fn allowed_values(&self, principal: &Principal, doctype: &str, field: &str) -> Allowed {
        if principal.is_system {
            return Allowed::All;
        }
        let facts = self.snapshot.facts(doctype);
        let target: Option<String> = facts.as_ref().and_then(|f| {
            f.links
                .iter()
                .find(|l| l.name == field)
                .and_then(|l| l.target.clone())
        });
        let path = format!("{doctype}.{field}");
        let mut union: BTreeSet<String> = BTreeSet::new();
        for a in &principal.assignments {
            if !self.assignment_holds(a, doctype, Right::Read, true) {
                continue;
            }
            let mut only: Option<BTreeSet<String>> = None;
            for up in sorted_scope(a) {
                if up.doctype.as_deref().is_some_and(|d| d != doctype) {
                    continue;
                }
                let t = target.as_deref().unwrap_or("");
                if up.restricts(doctype, &path, t) {
                    only.get_or_insert_with(BTreeSet::new)
                        .extend(up.allowed.iter().cloned());
                }
            }
            match only {
                None => return Allowed::All,
                Some(set) => union.extend(set),
            }
        }
        Allowed::Only(union.into_iter().collect())
    }
}
