//! Two rules about what a kind of user may hold or use (ADR-070 sections 1.1 and 5.6).
//!
//! # The portal rule (ADR-083 Barrier 3)
//!
//! A **consumer principal is not a [`User`](crate::User) and never reaches this code.** This is
//! the rule for **portal users** only: outside people who have a user record (a customer's
//! accountant, a supplier's clerk). A portal user may hold only a role of kind
//! [`RoleKind::Portal`] whose whole closure (ancestors and duties) carries nothing that is not
//! marked outside-exposed. [`validate_assignment`] is the one entry point every path that can
//! make or keep an assignment calls; it takes the role **resolved** (duties and inheritance
//! flattened), because a label the caller supplies is not evidence.
//!
//! | Path | Call site |
//! | --- | --- |
//! | Manual grant, delegated administration, emergency grant | `spot_iam_governance::GrantGate` |
//! | Assignment rules | `spot_iam_assign::plan_rules_guarded` |
//! | IdP group mapping | `spot_iam_assign::plan_idp_guarded`, `spot_iam_scim` `reconcile_idp` |
//! | Invitation acceptance | **not wired here**: the accept-invitation flow must call [`validate_assignment`] (follow-up task) |
//! | The engine's snapshot | `spot_iam_roles::build_snapshot_input_guarded` |

use spot_iam_roles::ResolvedRole;

pub use spot_iam_roles::RoleKind;

use crate::error::{self as e, IamError, IamErrors, Result};
use crate::model::UserType;

/// Refuses a desk role for a portal user: their access comes from party scoping alone.
///
/// This checks only the **label**, which the caller supplies. It cannot see a duty, so it is
/// no protection on its own; every path must call [`validate_assignment`] instead. Kept for
/// the callers and tests written before the allow-list.
pub fn validate_assignment_kind(user_type: UserType, role: RoleKind) -> Result<()> {
    if user_type == UserType::Portal && role == RoleKind::Desk {
        return Err(IamErrors::one(IamError::new(
            e::PORTAL_ROLE,
            "a portal user can hold no desk role; use a portal role",
        )));
    }
    Ok(())
}

/// The portal rule as an allow-list: a portal user may hold `role` only when its kind is
/// `Portal` and every duty it reaches, after inheritance, is marked outside-exposed; a direct
/// right (which belongs to no duty) and an unmarked duty are both refused. Every other user
/// type passes. The error names the role and the duty, never a user's data.
pub fn validate_assignment(user_type: UserType, role: &ResolvedRole) -> Result<()> {
    if user_type != UserType::Portal {
        return Ok(());
    }
    role.check_for_portal().map_err(|errors| {
        let list: Vec<IamError> = errors
            .errors()
            .iter()
            .map(|x| IamError::new(x.code, x.message.clone()))
            .collect();
        IamErrors::from_list(list).unwrap_or_else(|| {
            IamErrors::one(IamError::new(
                spot_iam_roles::codes::OUTSIDE_ROLE_KIND,
                "a portal user cannot hold this role",
            ))
        })
    })
}

/// How a principal proves who they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignInMethod {
    /// A local password (with a second factor).
    Password,
    /// A single sign-on provider.
    Sso,
    /// A one-time link by e-mail (with a second factor).
    MagicLink,
    /// An API key.
    ApiKey,
    /// OAuth client credentials.
    ClientCredentials,
}

/// Refuses a password for a service or an agent user: they authenticate with keys and
/// credentials only.
pub fn validate_sign_in_method(user_type: UserType, method: SignInMethod) -> Result<()> {
    if method == SignInMethod::Password && !user_type.may_use_password() {
        return Err(IamErrors::one(IamError::new(
            e::NO_PASSWORD,
            "service and agent users cannot sign in with a password",
        )));
    }
    Ok(())
}
