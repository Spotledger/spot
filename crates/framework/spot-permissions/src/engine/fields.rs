//! The field decisions (ADR-070 section 12): `field_access`, the field sets and the field
//! write check, as `impl PermissionEngine` split out of the document engine.
//!
//! The rules, as the method docs carry them: closed by default when protected, the strictest
//! group decides (intersection), the ladder (`update` implies `read`, `reveal` needs
//! `read_masked`), never above the DocType right, explicit `deny` wins, conditions, masks,
//! and no filtering on a field the reader may not see unmasked.

use std::collections::{BTreeMap, BTreeSet};

use spot_events_contract::lifecycle::Operation;

use super::{
    DocFacts, FIELD_DENIED, FieldChange, FieldDecision, FieldLevel, FieldSet, FieldViolation,
    PermissionEngine,
};
use crate::engine::rules::{condition_holds, dedup, sorted, sorted_roles, target_text};
use crate::model::{
    FieldFacts, FieldGrant, FieldRight, FieldTarget, GroupName, MaskFormat, Principal, Right, Role,
    TENANT_ADMINISTRATOR,
};
use crate::reason::Reason;

impl PermissionEngine {
    /// How the principal may see and change one field on one document (ADR-070 section 12).
    ///
    /// - **Closed by default**: a field in a protection group, or with a field grant on it, has
    ///   no access without a grant; a field in no group follows the DocType right.
    /// - **Several groups, least access wins**: the level and every flag are the intersection
    ///   of what each group grants; a missing grant in any one group closes the field.
    /// - **The ladder**: `update` implies `read`; `read` and `reveal` imply `read_masked`;
    ///   `reveal_bulk` implies `reveal`; `create` and `update` are separate, `report` is
    ///   separate from `read`.
    /// - **Never above the DocType right**: no field `update` without DocType `write`, no
    ///   field `read` without DocType `read`.
    /// - **Explicit `deny` wins** over every grant from every role of every contributing
    ///   assignment; between grants the most permissive wins.
    /// - A **condition** that fails makes its grant not apply (it is not a denial).
    #[must_use]
    pub fn field_access(
        &self,
        principal: &Principal,
        doctype: &str,
        field: &FieldFacts,
        doc: &DocFacts,
    ) -> FieldDecision {
        if principal.is_system {
            return FieldDecision {
                level: FieldLevel::ReadReveal,
                can_create: true,
                can_update: true,
                can_report: true,
                can_reveal: true,
                can_reveal_bulk: true,
                reasons: vec![Reason::System],
            };
        }
        let doc = &DocFacts {
            doctype: doctype.to_owned(),
            ..doc.clone()
        };
        let Some(facts) = self.snapshot.facts(doctype) else {
            return FieldDecision::closed(vec![Reason::NoGrant {
                doctype: doctype.to_owned(),
                wanted: "read".to_owned(),
            }]);
        };
        let evaluated = self.evaluate(principal, doc, &facts);
        let mut reasons: Vec<Reason> = Vec::new();
        for e in &evaluated {
            reasons.push(Reason::Assignment {
                id: e.assignment.id.clone(),
                covers: e.blocked.is_empty(),
            });
        }
        let Some(rights) = self.doc_rights(principal, doc, &evaluated) else {
            return FieldDecision::closed(reasons);
        };
        let posted = doc.state.as_deref() == Some("posted");
        let cancelled = doc.state.as_deref() == Some("cancelled");
        let write_right = if posted {
            Right::PostUpdate
        } else {
            Right::Write
        };
        let doc_read = rights.contains(&Right::Read);
        let doc_write = rights.contains(&write_right) && !cancelled;
        let doc_create = rights.contains(&Right::Create);
        let doc_report = rights.contains(&Right::Report);

        let field_has_grant =
            self.snapshot.field_grants().iter().any(|g| {
                g.doctype == doctype && g.target == FieldTarget::Field(field.path.clone())
            });
        if field.groups.is_empty() && !field_has_grant {
            reasons.push(Reason::FollowsDoctype {
                path: field.path.clone(),
            });
            return FieldDecision {
                level: if doc_read {
                    FieldLevel::Read
                } else {
                    FieldLevel::Hidden
                },
                can_create: doc_create,
                can_update: doc_write,
                can_report: doc_report,
                can_reveal: false,
                can_reveal_bulk: false,
                reasons: sorted(reasons),
            };
        }

        // Collect the grants of every contributing assignment's roles.
        let mut group_sets: BTreeMap<GroupName, BTreeSet<FieldRight>> = BTreeMap::new();
        for g in &field.groups {
            group_sets.entry(g.clone()).or_default();
        }
        let mut field_set: BTreeSet<FieldRight> = BTreeSet::new();
        let mut denied: BTreeSet<FieldRight> = BTreeSet::new();
        for e in evaluated.iter().filter(|e| e.blocked.is_empty()) {
            for role in sorted_roles(e.assignment) {
                if role.as_str() == TENANT_ADMINISTRATOR {
                    let all: BTreeSet<FieldRight> = FieldRight::all().into_iter().collect();
                    field_set.extend(all.iter().copied());
                    for set in group_sets.values_mut() {
                        set.extend(all.iter().copied());
                    }
                    reasons.push(Reason::FieldGrant {
                        assignment: e.assignment.id.clone(),
                        role: role.clone(),
                        target: format!("field:{}", field.path),
                        access: all.iter().map(|r| r.as_str().to_owned()).collect(),
                    });
                }
                for g in self.matching_field_grants(doctype, role, field) {
                    let target = target_text(&g.target);
                    let access: Vec<String> =
                        g.access.iter().map(|r| r.as_str().to_owned()).collect();
                    if !condition_holds(g.condition.as_ref(), principal, doc) {
                        reasons.push(Reason::ConditionFailed {
                            assignment: e.assignment.id.clone(),
                            role: role.clone(),
                            target,
                        });
                        continue;
                    }
                    if g.deny {
                        if g.access.is_empty() {
                            denied.extend(FieldRight::all());
                        } else {
                            denied.extend(g.access.iter().copied());
                        }
                        reasons.push(Reason::FieldDeny {
                            assignment: e.assignment.id.clone(),
                            role: role.clone(),
                            target,
                            access,
                        });
                        continue;
                    }
                    let closed = FieldRight::close(&g.access);
                    match &g.target {
                        FieldTarget::Field(_) => field_set.extend(closed),
                        FieldTarget::Group(name) => {
                            group_sets.entry(name.clone()).or_default().extend(closed);
                        }
                    }
                    reasons.push(Reason::FieldGrant {
                        assignment: e.assignment.id.clone(),
                        role: role.clone(),
                        target,
                        access,
                    });
                }
            }
        }

        // The strictest group decides: intersect what each group grants (a field grant adds
        // to every group's set, as the most permissive grant between grants wins).
        let mut effective: Option<BTreeSet<FieldRight>> = None;
        for (name, set) in &group_sets {
            let reach: BTreeSet<FieldRight> = set.union(&field_set).copied().collect();
            if reach.is_empty() && field.groups.len() > 1 {
                reasons.push(Reason::GroupNoGrant {
                    path: field.path.clone(),
                    group: name.0.clone(),
                });
            }
            effective = Some(match effective {
                None => reach,
                Some(prev) => prev.intersection(&reach).copied().collect(),
            });
        }
        let mut effective = effective.unwrap_or_else(|| field_set.clone());
        if effective.is_empty() && field.groups.len() <= 1 {
            reasons.push(Reason::FieldClosed {
                path: field.path.clone(),
            });
        }
        // An explicit deny removes the right and every right that carries it.
        effective.retain(|r| {
            !denied.contains(r) && !r.implied().iter().any(|implied| denied.contains(implied))
        });

        // A field right never exceeds the DocType right.
        let mut cap = |held: bool, missing: &str, wanted: bool| -> bool {
            if wanted && !held {
                reasons.push(Reason::DoctypeCap {
                    path: field.path.clone(),
                    missing: missing.to_owned(),
                });
            }
            wanted && held
        };
        let read = cap(
            doc_read,
            "read",
            effective.contains(&FieldRight::Read)
                || effective.contains(&FieldRight::ReadMasked)
                || effective.contains(&FieldRight::Reveal),
        );
        let can_update = cap(
            doc_write,
            write_right.as_str(),
            effective.contains(&FieldRight::Update),
        );
        let can_create = cap(
            doc_create,
            "create",
            effective.contains(&FieldRight::Create),
        );
        let can_report = cap(
            doc_report,
            "report",
            effective.contains(&FieldRight::Report),
        );
        let can_reveal = read && effective.contains(&FieldRight::Reveal);
        let can_reveal_bulk = read && effective.contains(&FieldRight::RevealBulk);
        let level = if !read {
            FieldLevel::Hidden
        } else if effective.contains(&FieldRight::Read) {
            if can_reveal {
                FieldLevel::ReadReveal
            } else {
                FieldLevel::Read
            }
        } else {
            FieldLevel::Masked(field.mask.unwrap_or(MaskFormat::Placeholder))
        };
        FieldDecision {
            level,
            can_create,
            can_update,
            can_report,
            can_reveal,
            can_reveal_bulk,
            reasons: sorted(dedup(reasons)),
        }
    }

    /// The field grants of `role` that speak about this field: on the field itself or on one
    /// of its groups.
    fn matching_field_grants<'s>(
        &'s self,
        doctype: &'s str,
        role: &'s Role,
        field: &'s FieldFacts,
    ) -> impl Iterator<Item = &'s FieldGrant> {
        self.snapshot.field_grants().iter().filter(move |g| {
            g.doctype == doctype
                && &g.role == role
                && match &g.target {
                    FieldTarget::Field(path) => path == &field.path,
                    FieldTarget::Group(name) => field.groups.contains(name),
                }
        })
    }

    /// The fields of `doctype` the principal may see (masked or not) on this document, in
    /// definition order.
    #[must_use]
    pub fn readable_fields(
        &self,
        principal: &Principal,
        doctype: &str,
        doc: &DocFacts,
    ) -> FieldSet {
        self.fields_where(principal, doctype, doc, |d| d.level != FieldLevel::Hidden)
    }

    /// The fields the principal may write for `op`: `create` for a create, `update` otherwise.
    #[must_use]
    pub fn writable_fields(
        &self,
        principal: &Principal,
        doctype: &str,
        op: Operation,
        doc: &DocFacts,
    ) -> FieldSet {
        let creating = op == Operation::Create;
        self.fields_where(principal, doctype, doc, move |d| {
            if creating { d.can_create } else { d.can_update }
        })
    }

    /// The fields the principal may use in list columns, reports, exports, print and search.
    #[must_use]
    pub fn reportable_fields(
        &self,
        principal: &Principal,
        doctype: &str,
        doc: &DocFacts,
    ) -> FieldSet {
        self.fields_where(principal, doctype, doc, |d| d.can_report)
    }

    fn fields_where(
        &self,
        principal: &Principal,
        doctype: &str,
        doc: &DocFacts,
        keep: impl Fn(&FieldDecision) -> bool,
    ) -> FieldSet {
        let Some(facts) = self.snapshot.facts(doctype) else {
            return FieldSet::default();
        };
        FieldSet {
            fields: facts
                .fields
                .iter()
                .filter(|f| keep(&self.field_access(principal, doctype, f, doc)))
                .map(|f| f.path.clone())
                .collect(),
        }
    }

    /// May the principal filter, sort, group or aggregate on this field? Only with `read`
    /// unmasked (the no-inference rule, ADR-070 section 12, rule 7): a masked or hidden field
    /// in a filter is refused so an aggregate or an ordered list cannot leak it. The query
    /// compiler (WP-PLT-04) calls this; it asks without a document, so a grant that depends on
    /// the document (a condition, `if_owner`) does not count.
    #[must_use]
    pub fn filter_ok(&self, principal: &Principal, doctype: &str, field: &str) -> bool {
        let facts = self.snapshot.facts(doctype);
        let field_facts = facts
            .as_deref()
            .and_then(|f| f.field(field))
            .cloned()
            .unwrap_or_else(|| FieldFacts::plain(field));
        let doc = DocFacts {
            doctype: doctype.to_owned(),
            ..DocFacts::default()
        };
        matches!(
            self.field_access(principal, doctype, &field_facts, &doc)
                .level,
            FieldLevel::Read | FieldLevel::ReadReveal
        )
    }

    /// The writes the principal may not make: a `Set` of a field it cannot create (on
    /// `Operation::Create`) or update, each as `permission.field_denied` with the path. An
    /// `Unchanged` echo is always accepted so edit forms work.
    #[must_use]
    pub fn check_write(
        &self,
        principal: &Principal,
        doctype: &str,
        op: Operation,
        doc: &DocFacts,
        changes: &[(String, FieldChange)],
    ) -> Vec<FieldViolation> {
        let writable = self.writable_fields(principal, doctype, op, doc);
        changes
            .iter()
            .filter(|(path, change)| *change == FieldChange::Set && !writable.contains(path))
            .map(|(path, _)| FieldViolation {
                path: path.clone(),
                code: FIELD_DENIED,
            })
            .collect()
    }
}
