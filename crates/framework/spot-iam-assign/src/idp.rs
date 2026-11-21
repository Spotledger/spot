//! The IdP group-to-role mapping (ADR-070 section 9, rule 2): an explicit table
//! `(idp_group -> role, scope)`. The IdP never edits roles; it only decides assignments.

use std::collections::BTreeSet;

use serde::Deserialize;
use spot_iam_roles::RoleName;
use spot_permissions::{AppliesTo, UserPermission};

use crate::error::{self as e, AssignError, AssignErrors};
use crate::facts::UserId;
use crate::ownership::ManagedRoles;
use crate::rules::RoleDirectory;

/// A group, as the IdP names it (an OIDC claim value, a SAML attribute value, a SCIM group).
pub type Group = String;

/// A fixed restriction of a mapping.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticRestriction {
    /// Restricts only documents of this DocType; none is every DocType.
    #[serde(default)]
    pub doctype: Option<String>,
    /// The link DocType (or select field path).
    pub link: String,
    /// The allowed record ids.
    pub allowed: Vec<String>,
    /// Also reach the descendants.
    #[serde(default)]
    pub include_descendants: bool,
}

/// One row of the table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRole {
    /// The IdP group.
    pub group: Group,
    /// The role it gives.
    pub role: RoleName,
    /// Where the role applies; empty means everywhere.
    #[serde(default)]
    pub scope: Vec<StaticRestriction>,
}

/// The validated mapping table.
#[derive(Clone, Debug, Default)]
pub struct GroupRoleMap {
    rows: Vec<GroupRole>,
}

impl GroupRoleMap {
    /// Validates the rows, reporting every problem at once: an unknown role, an emergency-only
    /// role, an empty group, a duplicate `(group, role)` row, a restriction with no values.
    pub fn load(rows: Vec<GroupRole>, roles: &RoleDirectory) -> Result<GroupRoleMap, AssignErrors> {
        let mut problems = Vec::new();
        let mut seen = BTreeSet::new();
        for row in &rows {
            let subject = format!("idp:{}", row.group);
            if row.group.trim().is_empty() {
                problems.push(AssignError::new(
                    e::INVALID_RULE,
                    "a mapping row needs a group name",
                ));
            }
            if !seen.insert((row.group.clone(), row.role.clone())) {
                problems.push(AssignError::about(
                    e::DUPLICATE_RULE,
                    &subject,
                    format!("two rows map this group to `{}`", row.role.as_str()),
                ));
            }
            roles.check(&row.role, &subject, &mut problems);
            for r in &row.scope {
                if r.link.trim().is_empty() || r.allowed.is_empty() {
                    problems.push(AssignError::about(
                        e::INVALID_RULE,
                        &subject,
                        "a restriction needs a link DocType and at least one allowed record",
                    ));
                }
            }
        }
        if let Some(errors) = AssignErrors::from_list(problems) {
            return Err(errors);
        }
        let mut rows = rows;
        rows.sort_by(|a, b| (&a.group, &a.role).cmp(&(&b.group, &b.role)));
        Ok(GroupRoleMap { rows })
    }

    /// The rows, by group, then role.
    pub fn rows(&self) -> &[GroupRole] {
        &self.rows
    }

    /// The roles this table manages, declared IdP-managed under `provider`: a manual change
    /// to any of them is refused.
    pub fn managed_roles(&self, provider: &str) -> ManagedRoles {
        let mut managed = ManagedRoles::new();
        for row in &self.rows {
            managed.declare(row.role.clone(), provider);
        }
        managed
    }

    pub(crate) fn scope_for(row: &GroupRole, user: &UserId) -> spot_iam_roles::Scope {
        spot_iam_roles::Scope {
            restrictions: row
                .scope
                .iter()
                .map(|r| {
                    let mut allowed = r.allowed.clone();
                    allowed.sort();
                    allowed.dedup();
                    UserPermission {
                        user: user.clone(),
                        doctype: r.doctype.clone(),
                        field_or_link_doctype: r.link.clone(),
                        allowed,
                        applies_to: AppliesTo::default(),
                        include_descendants: r.include_descendants,
                    }
                })
                .collect(),
        }
    }
}
