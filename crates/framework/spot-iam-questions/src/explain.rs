//! What a user may do, and who may do something: the first two answerable questions, each row
//! carrying its why-chain and the rule the decision core's `explain` produces
//! (ADR-070 section 8, rule 5).
//!
//! Every row names the assignment, the role (with its inheritance path), the duty, the
//! source, the scope and the rule. Nothing here decides a permission by itself: the verdicts
//! are the decision core's, and this module only attributes them to the assignment that
//! carried them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_iam_roles::{Assignment, ResolvedRole, RoleName, Source};
use spot_permissions::{Action, AssignmentContext, DocFacts, Principal, Right, UserPermission};

use crate::engine::{ScopeQuery, World};
use crate::error::QuestionsError;

/// One effective permission a user holds, with where it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct PermissionRow {
    /// The DocType.
    pub doctype: String,
    /// The right, in its stable text form (`read`, `submit`, `update_posted`, ...).
    pub right: String,
    /// The lifecycle action the right maps to, when it has one; the right's own name
    /// otherwise (`export`, `report`, `select`, ...).
    pub action: String,
    /// The assignment that carries it.
    pub assignment: String,
    /// The role the assignment holds.
    pub role: String,
    /// The roles from the assignment's role down to the one that defines the right.
    pub role_path: Vec<String>,
    /// The duty that grants it, when a duty does; `None` for a direct grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty: Option<String>,
    /// Where the assignment came from (`manual`, `rule`, `idp`, `emergency`).
    pub source: String,
    /// The assignment's scope restrictions.
    pub scope: Vec<UserPermission>,
    /// Whether the grant applies only to documents the user created.
    pub if_owner: bool,
    /// The exact rule the decision core produced for this assignment on this DocType: the
    /// machine explanation (stable across releases).
    pub rule: String,
}

/// A holder of one permission, with the same why-chain as [`PermissionRow`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Holder {
    /// The user who holds it.
    pub user: String,
    /// The assignment that carries it.
    pub assignment: String,
    /// The role the assignment holds.
    pub role: String,
    /// The roles from the assignment's role down to the one that defines the right.
    pub role_path: Vec<String>,
    /// The duty that grants it, when a duty does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty: Option<String>,
    /// Where the assignment came from.
    pub source: String,
    /// The assignment's scope restrictions.
    pub scope: Vec<UserPermission>,
    /// Whether the grant applies only to documents the user created.
    pub if_owner: bool,
    /// The exact rule the decision core produced for this holder.
    pub rule: String,
}

/// A permission a question is asked about: a DocType and a lifecycle action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct PermissionSpec {
    /// The DocType.
    pub doctype: String,
    /// The action, in its stable text form (`read`, `create`, `update`, `post`, `cancel`,
    /// `delete`, `rename`, `update_posted`, `correct`).
    pub action: String,
}

impl PermissionSpec {
    /// The spec for `doctype` and `action`.
    #[must_use]
    pub fn new(doctype: &str, action: &str) -> PermissionSpec {
        PermissionSpec {
            doctype: doctype.to_owned(),
            action: action.to_owned(),
        }
    }

    /// Parses `"doctype.action"`, the shape a caller (and the admin UI) writes.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::UnknownPermission`] when there is no `.`, the DocType part is blank,
    /// or the action part is not a lifecycle action.
    pub fn parse(text: &str) -> Result<PermissionSpec, QuestionsError> {
        let Some((doctype, action)) = text.split_once('.') else {
            return Err(unknown(text, "write the permission as `doctype.action`"));
        };
        if doctype.trim().is_empty() || doctype != doctype.trim() {
            return Err(unknown(text, "the DocType part must be non-blank"));
        }
        if Action::parse(action).is_none() {
            return Err(unknown(
                text,
                "the action part must be a lifecycle action (read, create, update, post, \
                 cancel, delete, rename, update_posted, correct)",
            ));
        }
        Ok(PermissionSpec {
            doctype: doctype.to_owned(),
            action: action.to_owned(),
        })
    }

    /// The lifecycle action this spec names.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::UnknownPermission`] when a spec built by hand names an unknown
    /// action.
    pub fn action(&self) -> Result<Action, QuestionsError> {
        Action::parse(&self.action).ok_or_else(|| {
            unknown(
                &format!("{}.{}", self.doctype, self.action),
                "the action is not a lifecycle action",
            )
        })
    }

    /// The spec's text, for the audit row.
    #[must_use]
    pub fn subject(&self) -> String {
        format!("{}.{}", self.doctype, self.action)
    }
}

fn unknown(permission: &str, detail: &str) -> QuestionsError {
    QuestionsError::UnknownPermission {
        permission: permission.to_owned(),
        detail: detail.to_owned(),
    }
}

/// The lifecycle action a right maps to, when it has one.
#[must_use]
pub fn action_of(right: Right) -> Option<Action> {
    use spot_events_contract::lifecycle::Operation;
    Some(match right {
        Right::Read => Action::Read,
        Right::Write => Action::Op(Operation::Update),
        Right::Create => Action::Op(Operation::Create),
        Right::Delete => Action::Op(Operation::Delete),
        Right::Submit => Action::Op(Operation::Post),
        Right::Cancel => Action::Op(Operation::Cancel),
        Right::Correct => Action::Correct,
        Right::PostUpdate => Action::Op(Operation::UpdatePosted),
        Right::Export
        | Right::Import
        | Right::Print
        | Right::Email
        | Right::Share
        | Right::Report
        | Right::Select => return None,
    })
}

/// The rights an action needs, mirroring the decision core: `correct` needs both `cancel` on
/// the original and `create` for the amendment.
#[must_use]
pub(crate) fn action_rights(action: Action) -> Vec<Right> {
    use spot_events_contract::lifecycle::Operation;
    match action {
        Action::Read => vec![Right::Read],
        Action::Correct => vec![Right::Cancel, Right::Create],
        Action::Op(Operation::Create) => vec![Right::Create],
        Action::Op(Operation::Update | Operation::Rename) => vec![Right::Write],
        Action::Op(Operation::Post) => vec![Right::Submit],
        Action::Op(Operation::Cancel) => vec![Right::Cancel],
        Action::Op(Operation::Delete) => vec![Right::Delete],
        Action::Op(Operation::UpdatePosted) => vec![Right::PostUpdate],
    }
}

/// The source of every assignment, by id.
fn sources_by_id(assignments: &[Assignment]) -> BTreeMap<&str, Source> {
    assignments
        .iter()
        .map(|a| (a.id.as_str(), a.source))
        .collect()
}

/// The text form of a source.
#[must_use]
pub fn source_token(source: Source) -> String {
    match source {
        Source::Manual => "manual",
        Source::Rule => "rule",
        Source::Idp => "idp",
        Source::Emergency => "emergency",
    }
    .to_owned()
}

/// Every effective permission `user` holds at the world's instant, one row per why-chain.
///
/// The rows are sorted on `(doctype, right, assignment, role_path, duty)`, so two calls over
/// the same model return the same rows in the same order.
#[must_use]
pub fn permissions(world: &World, assignments: &[Assignment], user: &str) -> Vec<PermissionRow> {
    let sources = sources_by_id(assignments);
    let mut rows: Vec<PermissionRow> = Vec::new();
    let Some(contexts) = world.input.assignments.get(user) else {
        return rows;
    };
    for context in contexts {
        for role in &context.roles {
            let name = RoleName(role.as_str().to_owned());
            let Some(resolved) = world.input.resolved.get(&name) else {
                continue;
            };
            let source = sources.get(context.id.as_str()).copied();
            if resolved.name.as_str() == spot_permissions::TENANT_ADMINISTRATOR {
                for doctype in world.doctypes() {
                    for right in Right::all() {
                        rows.push(PermissionRow {
                            doctype: doctype.clone(),
                            right: right.as_str().to_owned(),
                            action: action_of(right).map_or_else(
                                || right.as_str().to_owned(),
                                |a| a.as_str().to_owned(),
                            ),
                            assignment: context.id.clone(),
                            role: resolved.name.as_str().to_owned(),
                            role_path: vec![resolved.name.as_str().to_owned()],
                            duty: None,
                            source: source_token(source.unwrap_or(Source::Manual)),
                            scope: context.scope.clone(),
                            if_owner: false,
                            rule: rule_for(world, context, user, doctype, right),
                        });
                    }
                }
                continue;
            }
            collect_rows(world, context, resolved, source, user, &mut rows);
        }
    }
    rows.sort_by(|a, b| {
        (&a.doctype, &a.right, &a.assignment, &a.role_path, &a.duty).cmp(&(
            &b.doctype,
            &b.right,
            &b.assignment,
            &b.role_path,
            &b.duty,
        ))
    });
    rows
}

fn collect_rows(
    world: &World,
    context: &AssignmentContext,
    resolved: &ResolvedRole,
    source: Option<Source>,
    user: &str,
    rows: &mut Vec<PermissionRow>,
) {
    for grant in &resolved.grants {
        for right in grant.grant.rights.as_slice() {
            for chain in &grant.sources {
                rows.push(PermissionRow {
                    doctype: grant.grant.doctype.clone(),
                    right: right.as_str().to_owned(),
                    action: action_of(*right)
                        .map_or_else(|| right.as_str().to_owned(), |a| a.as_str().to_owned()),
                    assignment: context.id.clone(),
                    role: resolved.name.as_str().to_owned(),
                    role_path: chain
                        .role_path
                        .iter()
                        .map(|r| r.as_str().to_owned())
                        .collect(),
                    duty: chain.duty.as_ref().map(|d| d.as_str().to_owned()),
                    source: source_token(source.unwrap_or(Source::Manual)),
                    scope: context.scope.clone(),
                    if_owner: grant.grant.if_owner,
                    rule: rule_for(world, context, user, &grant.grant.doctype, *right),
                });
            }
        }
    }
}

/// The decision core's rule for one assignment on one DocType, as the stable machine
/// explanation. A right with no lifecycle action (export, report, ...) has no `can` question;
/// its rule is the decision core's `role_grant` code for the assignment and role.
fn rule_for(
    world: &World,
    context: &AssignmentContext,
    user: &str,
    doctype: &str,
    right: Right,
) -> String {
    let role = context
        .roles
        .first()
        .map_or_else(String::new, |r| r.as_str().to_owned());
    match action_of(right) {
        Some(action) => {
            let principal = Principal::user(user, vec![context.clone()]);
            let doc = DocFacts {
                doctype: doctype.to_owned(),
                created_by: Some(user.to_owned()),
                ..DocFacts::default()
            };
            world.engine.explain_machine(&principal, action, &doc)
        }
        None => format!("role_grant({}, {role})", context.id),
    }
}

/// Who can do `spec` in `scope`: every user and assignment the decision core allows the
/// action for, with the same why-chain [`permissions`] gives.
///
/// The rows are sorted on `(user, assignment, role_path, duty)`.
pub fn holders(
    world: &World,
    assignments: &[Assignment],
    spec: &PermissionSpec,
    scope: &ScopeQuery,
) -> Result<Vec<Holder>, QuestionsError> {
    let action = spec.action()?;
    let doc = scope.document(&spec.doctype, "(scope)");
    Ok(holders_for_document(world, assignments, action, &doc))
}

/// The holders of `action` on one document (`who_can_do`'s scope form and `who_can_see`'s
/// why-chain share this): every user and assignment the decision core allows the action for
/// against the document's own facts.
#[must_use]
pub(crate) fn holders_for_document(
    world: &World,
    assignments: &[Assignment],
    action: Action,
    doc: &crate::model::DocumentRecord,
) -> Vec<Holder> {
    let needed = action_rights(action);
    let sources = sources_by_id(assignments);
    let facts = doc.facts();
    let mut rows: Vec<Holder> = Vec::new();
    for user in world.users() {
        let Some(contexts) = world.input.assignments.get(&user) else {
            continue;
        };
        for context in contexts {
            let principal = Principal::user(&user, vec![context.clone()]);
            if !world.engine.can(&principal, action, &facts).allowed {
                continue;
            }
            for role in &context.roles {
                let name = RoleName(role.as_str().to_owned());
                let Some(resolved) = world.input.resolved.get(&name) else {
                    continue;
                };
                let source = sources.get(context.id.as_str()).copied();
                if resolved.name.as_str() == spot_permissions::TENANT_ADMINISTRATOR {
                    rows.push(Holder {
                        user: user.clone(),
                        assignment: context.id.clone(),
                        role: resolved.name.as_str().to_owned(),
                        role_path: vec![resolved.name.as_str().to_owned()],
                        duty: None,
                        source: source_token(source.unwrap_or(Source::Manual)),
                        scope: context.scope.clone(),
                        if_owner: false,
                        rule: rule_for(world, context, &user, &doc.doctype, needed[0]),
                    });
                    continue;
                }
                for grant in &resolved.grants {
                    if grant.grant.doctype != doc.doctype
                        || !needed.iter().all(|r| grant.grant.rights.contains(*r))
                    {
                        continue;
                    }
                    for chain in &grant.sources {
                        rows.push(Holder {
                            user: user.clone(),
                            assignment: context.id.clone(),
                            role: resolved.name.as_str().to_owned(),
                            role_path: chain
                                .role_path
                                .iter()
                                .map(|r| r.as_str().to_owned())
                                .collect(),
                            duty: chain.duty.as_ref().map(|d| d.as_str().to_owned()),
                            source: source_token(source.unwrap_or(Source::Manual)),
                            scope: context.scope.clone(),
                            if_owner: grant.grant.if_owner,
                            rule: rule_for(world, context, &user, &doc.doctype, needed[0]),
                        });
                    }
                }
            }
        }
    }
    rows.sort_by(|a, b| {
        (&a.user, &a.assignment, &a.role_path, &a.duty).cmp(&(
            &b.user,
            &b.assignment,
            &b.role_path,
            &b.duty,
        ))
    });
    rows.dedup();
    rows
}
