//! The decision engine: `can`, `field_access`, field sets, link checks, value restrictions,
//! explanations.
//!
//! The engine holds one `Arc`'d [`Snapshot`] and answers questions against it. It is pure:
//! every method takes everything it needs (principal, action, document facts), reads nothing
//! else, and returns values in a deterministic order.
//!
//! **Evaluation is per assignment** (ADR-070 section 3). An assignment whose scope does not
//! cover the document contributes nothing, no grant and no denial; rights are the union over
//! the contributing assignments' roles; a state rule, an explicit field `deny` and a user
//! permission that blocks every assignment are never overridden by any role, share or
//! assignment.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::prelude::Value;
use spot_events_contract::lifecycle::Operation;

use crate::engine::rules::{
    dedup, scope_blocks, share_grants, sorted, sorted_roles, state_refusal,
};
use crate::model::{
    AssignmentContext, MaskFormat, Principal, Right, Role, RoleGrant, ShareRight,
    TENANT_ADMINISTRATOR,
};
use crate::reason::Reason;
use crate::snapshot::{DoctypeFacts, Snapshot};

mod explain;
mod fields;
mod rules;
mod scope;

/// The code a rejected field write carries (ADR-070 section 12, rule 8).
pub const FIELD_DENIED: &str = "permission.field_denied";

/// One lifecycle action, or `Correct`, or `Read`.
///
/// The brief: "Action is a lifecycle `Operation` (create, update, post, cancel, delete,
/// rename, update_posted) or `Correct` or `Read`".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// One of the seven lifecycle operations.
    Op(Operation),
    /// Correct (ADR-42): cancel and amend in one transaction. Its rule needs both the
    /// `cancel` right (the original) and the `create` right (the amendment).
    Correct,
    /// Load or query a document: the `read` right.
    Read,
}

impl Action {
    /// The stable text form the reasons and the support view use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Op(op) => match op {
                Operation::Create => "create",
                Operation::Update => "update",
                Operation::Post => "post",
                Operation::Cancel => "cancel",
                Operation::Delete => "delete",
                Operation::Rename => "rename",
                Operation::UpdatePosted => "update_posted",
            },
            Action::Correct => "correct",
            Action::Read => "read",
        }
    }

    /// The action its stable text names (`as_str`'s inverse), including every lifecycle
    /// operation name. Unknown text gives `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Action> {
        let op = match text {
            "create" => Operation::Create,
            "update" => Operation::Update,
            "post" => Operation::Post,
            "cancel" => Operation::Cancel,
            "delete" => Operation::Delete,
            "rename" => Operation::Rename,
            "update_posted" => Operation::UpdatePosted,
            "correct" => return Some(Action::Correct),
            "read" => return Some(Action::Read),
            _ => return None,
        };
        Some(Action::Op(op))
    }
}

/// The facts about one document a decision is asked against.
///
/// Everything state-related the engine may consult, and nothing else: `created_by` (for the
/// owner rules), the lifecycle state, and the field values the value restrictions read.
/// The engine never loads a document itself.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct DocFacts {
    /// The DocType's storage name.
    pub doctype: String,
    /// The record id text, when the document has one (a draft being created does not).
    pub id: Option<String>,
    /// Who created the document (`created_by`), by user name.
    pub created_by: Option<String>,
    /// The lifecycle state: `draft`, `posted`, `cancelled`; `None` for DocTypes without one.
    pub state: Option<String>,
    /// The document's values, by field name — the link and select values the user
    /// permissions restrict.
    pub values: BTreeMap<String, Value>,
}

impl DocFacts {
    /// Whether the acting principal created this document.
    #[must_use]
    pub fn owned_by(&self, principal: &Principal) -> bool {
        match (&self.created_by, principal.is_system) {
            (Some(creator), false) => !principal.user.is_empty() && creator == &principal.user,
            _ => false,
        }
    }
}

/// The outcome of one `can` question: the verdict and **every** rule that spoke.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    /// Whether the action is allowed.
    pub allowed: bool,
    /// Every reason that granted or denied, in deterministic order (kind, then role/field).
    pub reasons: Vec<Reason>,
}

impl Decision {
    /// The reason codes, as the matrix's expected column reads them: `role_grant`,
    /// `share`, `user_permission_blocked`, `posted_state`, … in the same deterministic order.
    #[must_use]
    pub fn codes(&self) -> Vec<String> {
        self.reasons.iter().map(Reason::code).collect()
    }

    /// Whether a kind of reason spoke (the matrix matches on kinds, so detail stays fluid).
    #[must_use]
    pub fn has_kind(&self, kind: &str) -> bool {
        self.reasons.iter().any(|r| r.kind() == kind)
    }
}

/// A field set: the field names that passed a check, in definition order.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FieldSet {
    /// The fields, in DocType definition order.
    pub fields: Vec<String>,
}

impl FieldSet {
    /// Whether `field` is in the set.
    #[must_use]
    pub fn contains(&self, field: &str) -> bool {
        self.fields.iter().any(|f| f == field)
    }

    /// How many fields the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// A link (or select value) outside the acting user's allowed set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The field path: `customer`, or `items[1].item` for a child-table row (idx from 1,
    /// the same shape the app API's `FieldPath` uses).
    pub path: String,
    /// The value that is outside the allowed set, as text.
    pub value: String,
}

/// What a user may enter in one restricted field.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Allowed {
    /// Everything: no restriction reached this field.
    #[default]
    All,
    /// Only these records or values.
    Only(Vec<String>),
}

/// How visible a field is to one principal on one document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldLevel {
    /// The field does not exist for this reader.
    Hidden,
    /// The reader sees the masked form; the real value never leaves the server.
    Masked(MaskFormat),
    /// The reader sees the value.
    Read,
    /// The reader sees the value and may reveal masked values.
    ReadReveal,
}

/// The outcome of one field question: the level, the write and report flags, and every rule
/// that spoke.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldDecision {
    /// How visible the field is.
    pub level: FieldLevel,
    /// May set the field when creating the document.
    pub can_create: bool,
    /// May change the field.
    pub can_update: bool,
    /// May use the field in list columns, reports, exports, print and search results.
    pub can_report: bool,
    /// May reveal the unmasked value, one document at a time.
    pub can_reveal: bool,
    /// May reveal in bulk (back-end services only).
    pub can_reveal_bulk: bool,
    /// Every rule that spoke, in deterministic order.
    pub reasons: Vec<Reason>,
}

impl FieldDecision {
    fn closed(reasons: Vec<Reason>) -> FieldDecision {
        FieldDecision {
            level: FieldLevel::Hidden,
            can_create: false,
            can_update: false,
            can_report: false,
            can_reveal: false,
            can_reveal_bulk: false,
            reasons: sorted(reasons),
        }
    }

    /// The reason codes in the deterministic order the engine returns them.
    #[must_use]
    pub fn codes(&self) -> Vec<String> {
        self.reasons.iter().map(Reason::code).collect()
    }

    /// Whether a kind of reason spoke.
    #[must_use]
    pub fn has_kind(&self, kind: &str) -> bool {
        self.reasons.iter().any(|r| r.kind() == kind)
    }
}

/// What a write does to one field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldChange {
    /// The payload sets or changes the field.
    Set,
    /// The payload echoes the stored value back unchanged; accepted for a hidden or masked
    /// field so edit forms work.
    Unchanged,
}

/// A write the principal may not make: `permission.field_denied` with the field path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldViolation {
    /// The field path.
    pub path: String,
    /// Always [`FIELD_DENIED`].
    pub code: &'static str,
}

/// One assignment as evaluated against one document.
pub(crate) struct Evaluated<'a> {
    pub(crate) assignment: &'a AssignmentContext,
    /// The `(field path, value)` pairs its scope blocks; empty when it covers the document.
    pub(crate) blocked: Vec<(String, String)>,
}

/// The permission engine: every decision against one immutable snapshot.
#[derive(Clone, Debug)]
pub struct PermissionEngine {
    snapshot: Arc<Snapshot>,
}

impl PermissionEngine {
    /// An engine over a snapshot. A new snapshot means a new engine.
    #[must_use]
    pub fn new(snapshot: Arc<Snapshot>) -> PermissionEngine {
        PermissionEngine { snapshot }
    }

    /// The snapshot the engine reads.
    #[must_use]
    pub fn snapshot(&self) -> &Arc<Snapshot> {
        &self.snapshot
    }

    // ----------------------------------------------------------------- documents

    /// May `principal` do `action` on a document with these facts?
    ///
    /// The rules, in the order the reasons are produced:
    ///
    /// 1. **System** (`Actor::System` invocations): allowed, one `system` reason.
    /// 2. **State rules** (denials no role can lift): a posted document is touched only
    ///    through `update_posted` (and read); a cancelled one only read; a draft is not
    ///    cancelled or amended; `update_posted` needs a posted document (ADR-42).
    /// 3. **Assignments**: each is evaluated on its own. One whose scope blocks the document
    ///    contributes nothing; when every assignment is blocked the decision is a final denial
    ///    that no share lifts.
    /// 4. **Rights**: the union over the contributing assignments' roles and live shares,
    ///    with `if_owner` grants applying only to the owner.
    ///
    /// `correct` composes cancel on the posted original **and** create for the amendment
    /// (ADR-42, WP-DOC-04): it runs both operations in one transaction, so it needs both
    /// rights.
    #[must_use]
    pub fn can(&self, principal: &Principal, action: Action, doc: &DocFacts) -> Decision {
        if principal.is_system {
            return Decision {
                allowed: true,
                reasons: vec![Reason::System],
            };
        }
        let Some(facts) = self.snapshot.facts(&doc.doctype) else {
            return Decision {
                allowed: false,
                reasons: vec![Reason::NoGrant {
                    doctype: doc.doctype.clone(),
                    wanted: "*".to_owned(),
                }],
            };
        };
        let mut reasons: Vec<Reason> = Vec::new();
        if let Some(refusal) = state_refusal(action, doc) {
            reasons.push(refusal);
            return Decision {
                allowed: false,
                reasons: sorted(reasons),
            };
        }

        let evaluated = self.evaluate(principal, doc, &facts);
        let blocked_all = !evaluated.is_empty() && evaluated.iter().all(|e| !e.blocked.is_empty());
        for e in &evaluated {
            reasons.push(Reason::Assignment {
                id: e.assignment.id.clone(),
                covers: e.blocked.is_empty(),
            });
            for (field, value) in &e.blocked {
                reasons.push(Reason::UserPermissionBlocked {
                    assignment: e.assignment.id.clone(),
                    field: field.clone(),
                    value: value.clone(),
                });
            }
        }
        if blocked_all {
            return Decision {
                allowed: false,
                reasons: sorted(reasons),
            };
        }

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
        let mut allowed = true;
        for right in needed {
            let held = self.holds(principal, doc, &evaluated, *right, &mut reasons);
            if !held {
                allowed = false;
                reasons.push(Reason::NoGrant {
                    doctype: doc.doctype.clone(),
                    wanted: right.as_str().to_owned(),
                });
            }
        }
        Decision {
            allowed,
            reasons: sorted(dedup(reasons)),
        }
    }

    /// Evaluates every assignment against the document: which of them its scope blocks.
    fn evaluate<'a>(
        &self,
        principal: &'a Principal,
        doc: &DocFacts,
        facts: &DoctypeFacts,
    ) -> Vec<Evaluated<'a>> {
        let mut assignments: Vec<&AssignmentContext> = principal.assignments.iter().collect();
        assignments.sort_by(|a, b| a.id.cmp(&b.id));
        assignments
            .into_iter()
            .map(|assignment| Evaluated {
                assignment,
                blocked: scope_blocks(assignment, doc, facts),
            })
            .collect()
    }

    /// Whether the contributing assignments' roles, or a share, give `right`; pushes one
    /// reason per grant or share that spoke.
    fn holds(
        &self,
        principal: &Principal,
        doc: &DocFacts,
        evaluated: &[Evaluated<'_>],
        right: Right,
        reasons: &mut Vec<Reason>,
    ) -> bool {
        let owner = doc.owned_by(principal);
        let mut held = false;
        for e in evaluated.iter().filter(|e| e.blocked.is_empty()) {
            for role in sorted_roles(e.assignment) {
                if role.as_str() == TENANT_ADMINISTRATOR {
                    reasons.push(Reason::RoleGrant {
                        assignment: e.assignment.id.clone(),
                        role: role.clone(),
                        owner_applies: false,
                        rights: vec![right.as_str().to_owned()],
                    });
                    held = true;
                    continue;
                }
                for grant in self.grants_of(&doc.doctype, role, right) {
                    if grant.if_owner && !owner {
                        reasons.push(Reason::OwnerOnly {
                            assignment: e.assignment.id.clone(),
                            role: role.clone(),
                            would_allow: vec![right.as_str().to_owned()],
                        });
                        continue;
                    }
                    reasons.push(Reason::RoleGrant {
                        assignment: e.assignment.id.clone(),
                        role: role.clone(),
                        owner_applies: grant.if_owner,
                        rights: vec![right.as_str().to_owned()],
                    });
                    held = true;
                }
            }
        }
        let shared = self.share_rights(principal, doc, right);
        if !shared.is_empty() {
            reasons.push(Reason::Share { rights: shared });
            held = true;
        }
        held
    }

    /// The DocType grants of `role` on `doctype` that carry `right`.
    fn grants_of<'s>(
        &'s self,
        doctype: &'s str,
        role: &'s Role,
        right: Right,
    ) -> impl Iterator<Item = &'s RoleGrant> {
        self.snapshot
            .grants()
            .iter()
            .filter(move |g| g.doctype == doctype && &g.role == role && g.rights.contains(right))
    }

    /// The share rights the principal holds on this document that carry `right`.
    fn share_rights(&self, principal: &Principal, doc: &DocFacts, right: Right) -> Vec<ShareRight> {
        let Some(id) = doc
            .id
            .as_deref()
            .and_then(|i| crate::ids::parse_record_id(i).ok())
        else {
            return vec![];
        };
        let mut out = BTreeSet::new();
        for share in self.snapshot.shares() {
            if share.user == principal.user && share.doc == id {
                for sr in &share.rights {
                    if share_grants(*sr, right) {
                        out.insert(*sr);
                    }
                }
            }
        }
        ShareRight::sorted(out)
    }

    /// The DocType rights the principal holds on a document: the union over the contributing
    /// assignments' roles (an `if_owner` grant counting only for the owner) and live shares.
    /// `None` when every assignment's scope blocks the document.
    fn doc_rights(
        &self,
        principal: &Principal,
        doc: &DocFacts,
        evaluated: &[Evaluated<'_>],
    ) -> Option<BTreeSet<Right>> {
        if !evaluated.is_empty() && evaluated.iter().all(|e| !e.blocked.is_empty()) {
            return None;
        }
        let owner = doc.owned_by(principal);
        let mut set = BTreeSet::new();
        for e in evaluated.iter().filter(|e| e.blocked.is_empty()) {
            for role in &e.assignment.roles {
                if role.as_str() == TENANT_ADMINISTRATOR {
                    set.extend(Right::all());
                    continue;
                }
                for g in self.snapshot.grants() {
                    if g.doctype == doc.doctype && &g.role == role && (!g.if_owner || owner) {
                        set.extend(g.rights.as_slice().iter().copied());
                    }
                }
            }
        }
        for right in Right::all() {
            if !self.share_rights(principal, doc, right).is_empty() {
                set.insert(right);
            }
        }
        Some(set)
    }
}
