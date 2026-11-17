//! The portal rule at the host's own call sites (Barrier 3 of ADR-083, WP-IAM-11.T16).
//!
//! A **portal** user (an outside person with a user record) may hold only a role whose kind is
//! portal and whose every duty is marked outside-exposed. `spot-iam-roles` owns the rule
//! ([`OutsideGuard`]); this module builds the guard from the site's stored `role` and `user`
//! documents and asks it at the two places the operator host writes or keeps a role assignment:
//!
//! * [`check_new_assignments`], called by `users::write_user` before each `role_assignment` is
//!   inserted for a portal user (invitation), and
//! * [`check_user_assignments`], called by `users::activate` (the moment an invited user
//!   accepts and becomes `active`) for the user's active assignments.
//!
//! Internal, service and agent users are unaffected: the guard passes everyone who is not a
//! portal user, and `write_user` takes a user type that defaults to internal.
//!
//! # What storage knows
//!
//! The platform `role` DocType records a role's name and the names of its duties (the `duties`
//! child table). It records no duty definitions, so the duties' outside-exposed marks cannot be
//! read from storage, and it records no role kind. [`guard_from`] therefore builds a full
//! [`Registry`] only when a caller can supply the duty definitions; the host's storage loader
//! ([`guard_for`]) cannot, and falls back to [`OutsideGuard::from_roles`], which **fails
//! closed**: a stored role that includes any duty (or is not of portal kind) is refused for a
//! portal user. It never falls back to [`OutsideGuard::none`].
//!
//! Errors name the role and the duty, never the user's e-mail address.

use crate::{Error, Result, error};
use spot_iam_roles::{Duty, DutyRef, IamErrors, OutsideGuard, Registry, Role, RoleKind, RoleName};
use spot_platform_app::doctype;
use spot_storage_contract::{
    CmpOp, DocumentData, DocumentStore, Expr, Name, Query, QueryExecutor, QueryPath, RecordId,
    RowLock, Selection, Value,
};
use std::collections::BTreeMap;

/// The user type stored for an outside (portal) person.
pub const PORTAL: &str = "portal";

/// The most rows one listing here reads. A site with more roles or portal users than this
/// fails closed: the roles beyond the cap are unknown to the guard and so refused.
const LIMIT: u32 = 100_000;

fn name(text: &str) -> Result<Name> {
    Name::new(text).map_err(error)
}

fn text_of(fields: &BTreeMap<Name, Value>, field: &str) -> Option<String> {
    fields.iter().find_map(|(n, v)| match v {
        Value::Text(t) if n.as_str() == field => Some(t.clone()),
        _ => None,
    })
}

/// The ids of every document of `entity` matching `filter` (a `field = value` test), or all.
async fn ids(
    tx: &mut impl QueryExecutor,
    entity: &str,
    filter: Option<(&str, Value)>,
) -> Result<Vec<RecordId>> {
    let alias = name("t")?;
    let mut query = Query::from(&name(entity)?, &alias, QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::field(&alias, &name("id")?),
        alias: name("v")?,
    }];
    if let Some((field, value)) = filter {
        query.filter = Some(Expr::Cmp(
            Box::new(Expr::field(&alias, &name(field)?)),
            CmpOp::Eq,
            Box::new(Expr::Value(value)),
        ));
    }
    query.limit = Some(LIMIT);
    let rows = tx.query(&query).await.map_err(error)?;
    Ok(rows
        .rows
        .into_iter()
        .flatten()
        .filter_map(|v| match v {
            Value::Id(id) => Some(id),
            _ => None,
        })
        .collect())
}

/// A stored role document as the role model: its name, its duty names and, when the document
/// carries a `kind` field saying `portal`, the portal kind. Anything else is a desk role, so a
/// stored role is never assumed to be a portal role. A document with no role name is skipped
/// (its role stays unknown to the guard and is refused).
pub fn roles_from_documents(documents: &[DocumentData]) -> Vec<Role> {
    documents
        .iter()
        .filter_map(|doc| {
            let role_name = text_of(&doc.fields, "role_name").filter(|n| !n.is_empty())?;
            let mut role = Role::custom(&role_name);
            if let Some((_, rows)) = doc.children.iter().find(|(n, _)| n.as_str() == "duties") {
                role.duties = rows
                    .iter()
                    .filter_map(|row| text_of(&row.fields, "duty"))
                    .filter(|d| !d.is_empty())
                    .map(|d| DutyRef::named(&d))
                    .collect();
            }
            if text_of(&doc.fields, "kind").as_deref() == Some("portal") {
                role.kind = RoleKind::Portal;
            }
            Some(role)
        })
        .collect()
}

/// The guard over `roles` for `portal_users`. With the duty definitions (`duties`) a registry
/// is built and the duty-level check applies; when there are none, or the registry cannot be
/// built, the guard is [`OutsideGuard::from_roles`] (fail closed). Never [`OutsideGuard::none`].
pub fn guard_from(
    roles: Vec<Role>,
    duties: Option<Vec<Duty>>,
    portal_users: Vec<String>,
) -> OutsideGuard {
    if let Some(duties) = duties
        && let Ok(registry) = Registry::new(duties, roles.clone())
    {
        return OutsideGuard::new(&registry, portal_users);
    }
    OutsideGuard::from_roles(&roles, portal_users)
}

/// The site's guard: its stored roles and the `user` documents of type `portal`.
pub async fn guard_for<T>(tx: &mut T) -> Result<OutsideGuard>
where
    T: DocumentStore + QueryExecutor,
{
    let entity = name(doctype::ROLE)?;
    let mut documents = Vec::new();
    for id in ids(tx, doctype::ROLE, None).await? {
        if let Some(doc) = tx.load(&entity, id, RowLock::None).await.map_err(error)? {
            documents.push(doc.data);
        }
    }
    let portal = ids(
        tx,
        doctype::USER,
        Some(("user_type", Value::Text(PORTAL.to_owned()))),
    )
    .await?
    .into_iter()
    .map(|id| id.to_string())
    .collect();
    // The duty definitions (and so their outside-exposed marks) are not stored on the site:
    // `None` takes the fail-closed path.
    Ok(guard_from(roles_from_documents(&documents), None, portal))
}

/// The catalogue code of the refusal below.
const CODE_ROLE_REFUSED: &str = "cli.invitation_role_refused";

/// The CLI refusal: names the role and the first duty the stored role lists, and the rule's
/// own reason. It never carries a user's e-mail address.
fn refusal(role: &str, duty: Option<&str>, why: &IamErrors) -> Error {
    let reason = why
        .errors()
        .first()
        .map_or_else(|| "refused".to_owned(), |e| e.message.clone());
    Error(format!(
        "{CODE_ROLE_REFUSED}: role `{role}` (duty `{}`) cannot be held by a portal \
         user: {reason}",
        duty.unwrap_or("none")
    ))
}

/// The first duty of the stored role called `role`, for the refusal text.
async fn first_duty<T>(tx: &mut T, role: &str) -> Option<String>
where
    T: DocumentStore + QueryExecutor,
{
    let entity = name(doctype::ROLE).ok()?;
    let id = ids(
        tx,
        doctype::ROLE,
        Some(("role_name", Value::Text(role.to_owned()))),
    )
    .await
    .ok()?
    .into_iter()
    .next()?;
    let doc = tx.load(&entity, id, RowLock::None).await.ok()??;
    roles_from_documents(std::slice::from_ref(&doc.data))
        .into_iter()
        .next()?
        .duties
        .first()
        .map(|d| d.name.as_str().to_owned())
}

/// Before an invitation writes assignments for a portal user: every role in `roles` must pass.
/// The first refusal aborts (the caller rolls the whole invitation back).
pub async fn check_new_assignments<T>(tx: &mut T, user_type: &str, roles: &[String]) -> Result<()>
where
    T: DocumentStore + QueryExecutor,
{
    if user_type != PORTAL || roles.is_empty() {
        return Ok(());
    }
    let guard = guard_for(tx).await?;
    for role in roles {
        if let Err(why) = guard.check_role(&RoleName::from(role.as_str())) {
            let duty = first_duty(tx, role).await;
            return Err(refusal(role, duty.as_deref(), &why));
        }
    }
    Ok(())
}

/// At acceptance: the user's **active** assignments are checked against the guard. A user that
/// is not a portal user passes. The account is activated by the caller only when this is `Ok`.
pub async fn check_user_assignments<T>(tx: &mut T, user: RecordId) -> Result<()>
where
    T: DocumentStore + QueryExecutor,
{
    let guard = guard_for(tx).await?;
    let who = user.to_string();
    if !guard.is_portal(&who) {
        return Ok(());
    }
    let roles = name(doctype::ROLE)?;
    let assignments = name(doctype::ROLE_ASSIGNMENT)?;
    for id in ids(
        tx,
        doctype::ROLE_ASSIGNMENT,
        Some(("user", Value::Id(user))),
    )
    .await?
    {
        let Some(assignment) = tx
            .load(&assignments, id, RowLock::None)
            .await
            .map_err(error)?
        else {
            continue;
        };
        if text_of(&assignment.data.fields, "state").as_deref() != Some("active") {
            continue;
        }
        let role_id = assignment.data.fields.iter().find_map(|(n, v)| match v {
            Value::Id(r) if n.as_str() == "role" => Some(*r),
            _ => None,
        });
        // An assignment whose role cannot be read is refused: unknown is closed.
        let role = match role_id {
            Some(r) => tx.load(&roles, r, RowLock::None).await.map_err(error)?,
            None => None,
        };
        let role_name = role
            .as_ref()
            .and_then(|d| text_of(&d.data.fields, "role_name"))
            .unwrap_or_default();
        if let Err(why) = guard.check(&who, &RoleName::from(role_name.as_str())) {
            let duty = first_duty(tx, &role_name).await;
            return Err(refusal(
                if role_name.is_empty() {
                    "unknown"
                } else {
                    &role_name
                },
                duty.as_deref(),
                &why,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_iam_roles::{Duty, DutyName, Version};
    use spot_storage_contract::ChildRow;

    fn role_doc(role_name: &str, duties: &[&str], kind: Option<&str>) -> DocumentData {
        let mut fields = BTreeMap::new();
        fields.insert(
            Name::new("role_name").unwrap(),
            Value::Text(role_name.into()),
        );
        if let Some(kind) = kind {
            fields.insert(Name::new("kind").unwrap(), Value::Text(kind.into()));
        }
        let rows = duties
            .iter()
            .map(|d| {
                let mut f = BTreeMap::new();
                f.insert(Name::new("duty").unwrap(), Value::Text((*d).into()));
                ChildRow {
                    id: RecordId::new_v7(),
                    fields: f,
                }
            })
            .collect();
        let mut children = BTreeMap::new();
        children.insert(Name::new("duties").unwrap(), rows);
        DocumentData {
            entity: Name::new("role").unwrap(),
            id: RecordId::new_v7(),
            fields,
            children,
        }
    }

    fn duty(n: &str, exposed: bool) -> Duty {
        Duty {
            name: DutyName::from(n),
            version: Version(1),
            grants: vec![],
            field_grants: vec![],
            app_permissions: vec![],
            outside_exposed: exposed,
        }
    }

    #[test]
    fn stored_roles_are_desk_roles_unless_they_say_portal() {
        let roles = roles_from_documents(&[
            role_doc("Clerk", &["post"], None),
            role_doc("Viewer", &["view"], Some("portal")),
        ]);
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].kind, RoleKind::Desk);
        assert_eq!(roles[0].duties.len(), 1);
        assert_eq!(roles[1].kind, RoleKind::Portal);
    }

    #[test]
    fn a_portal_role_with_only_exposed_duties_is_accepted_with_a_registry() {
        let roles = roles_from_documents(&[role_doc("Viewer", &["view"], Some("portal"))]);
        let guard = guard_from(roles, Some(vec![duty("view", true)]), vec!["u1".into()]);
        assert!(guard.check("u1", &RoleName::from("Viewer")).is_ok());
    }

    #[test]
    fn an_unexposed_duty_is_refused_with_a_registry() {
        let roles = roles_from_documents(&[role_doc("Viewer", &["view"], Some("portal"))]);
        let guard = guard_from(roles, Some(vec![duty("view", false)]), vec!["u1".into()]);
        assert!(guard.check("u1", &RoleName::from("Viewer")).is_err());
    }

    #[test]
    fn without_duty_definitions_a_duty_bearing_role_is_refused_fail_closed() {
        let roles = roles_from_documents(&[role_doc("Viewer", &["view"], Some("portal"))]);
        let guard = guard_from(roles, None, vec!["u1".into()]);
        assert!(guard.check("u1", &RoleName::from("Viewer")).is_err());
        // An internal user is not protected by the guard.
        assert!(
            guard
                .check("someone-else", &RoleName::from("Viewer"))
                .is_ok()
        );
    }

    #[test]
    fn an_unknown_role_is_refused_for_a_portal_user() {
        let guard = guard_from(vec![], None, vec!["u1".into()]);
        assert!(guard.check("u1", &RoleName::from("Nope")).is_err());
    }

    #[test]
    fn the_refusal_names_the_role_and_the_duty_and_no_address() {
        let guard = guard_from(
            roles_from_documents(&[role_doc("Clerk", &["post"], None)]),
            None,
            vec![],
        );
        let why = guard.check_role(&RoleName::from("Clerk")).unwrap_err();
        let text = refusal("Clerk", Some("post"), &why).0;
        assert!(text.contains("Clerk") && text.contains("post"), "{text}");
        assert!(!text.contains('@'), "{text}");
    }
}
