//! The tenant sign-in policy at the password door (WP-IAM-05.T12, ADR-070 sections 5 and 7).
//!
//! `spot-sso` decides what a tenant's policy means; nothing called it on the password path, so a
//! tenant set to `sso_required` still accepted a password. [`SigninPolicyPort`] is the small seam
//! the login service asks **before it verifies a password**; the host implements it over the
//! settings store (`services/signin_policy.rs`).
//!
//! Rules:
//!
//! - `sso_required` refuses a password sign-in with `login.sso_required` (the contract's
//!   `sso.required`), for everyone except the tenant's **break-glass administrators**;
//! - an error from the port is treated as `sso_required` with nobody exempt: a policy that cannot
//!   be read **never** falls back to `local`;
//! - an unknown name is refused exactly like a known non-break-glass one, before any hashing;
//! - API keys and the SCIM credential are service credentials and are **not** governed by this
//!   policy (they do not pass through the password door; a test pins it so nobody "fixes" it);
//! - a session may never move to a weaker method ([`ensure_not_weaker`]).

use std::sync::Arc;

use spot_iam_access::{AuthMethod, MethodError, MethodGuard, TargetSignIn};
use spot_storage_contract::RecordId;

use crate::ports::BoxFuture;

/// Which sign-in methods a tenant allows (mirrors `spot_sso::SignInMode`, which this crate does
/// not depend on).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigninMode {
    /// Local passwords only.
    Local,
    /// Passwords and single sign-on.
    SsoOptional,
    /// Single sign-on only, except the break-glass administrators.
    SsoRequired,
}

/// The tenant's policy as the password door needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigninPolicy {
    /// The mode.
    pub mode: SigninMode,
    /// The break-glass administrators (the only password bypass under `SsoRequired`).
    pub break_glass: Vec<RecordId>,
}

impl SigninPolicy {
    /// The policy of a tenant without single sign-on.
    #[must_use]
    pub fn local() -> Self {
        Self {
            mode: SigninMode::Local,
            break_glass: Vec::new(),
        }
    }
}

/// The policy could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyUnavailable;

/// Where the password door reads the tenant policy.
pub trait SigninPolicyPort: Send + Sync + 'static {
    /// The policy of `tenant`.
    fn policy<'a>(
        &'a self,
        tenant: &'a str,
    ) -> BoxFuture<'a, Result<SigninPolicy, PolicyUnavailable>>;
}

/// The port of a site that has no single sign-on: every password is allowed. A site with the
/// policy module uses its port instead; this is never a fallback for an error.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowLocal;

impl SigninPolicyPort for AllowLocal {
    fn policy<'a>(
        &'a self,
        _tenant: &'a str,
    ) -> BoxFuture<'a, Result<SigninPolicy, PolicyUnavailable>> {
        Box::pin(async { Ok(SigninPolicy::local()) })
    }
}

/// What the password door does with an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasswordDecision {
    /// Verify the password as usual.
    Allow,
    /// Verify the password; the user is a break-glass administrator (record `break_glass`).
    BreakGlass,
    /// Refuse with `login.sso_required` before any hashing.
    Refuse,
}

/// Decides a password attempt for `user` (`None` for a name that belongs to nobody) under
/// `policy`; `None` for the policy means it could not be read (refuse, nobody exempt).
#[must_use]
pub fn decide_password(policy: Option<&SigninPolicy>, user: Option<RecordId>) -> PasswordDecision {
    let Some(policy) = policy else {
        return PasswordDecision::Refuse;
    };
    match policy.mode {
        SigninMode::Local | SigninMode::SsoOptional => PasswordDecision::Allow,
        SigninMode::SsoRequired => match user {
            Some(user) if policy.break_glass.contains(&user) => PasswordDecision::BreakGlass,
            _ => PasswordDecision::Refuse,
        },
    }
}

/// Whether a session at `current` may be refreshed, stepped up or re-issued through `target`.
///
/// # Errors
///
/// [`MethodError::Weaker`] when `target` ranks below `current`: step-up for an SSO session needs
/// the identity provider (or a stronger factor), never the local password alone.
pub fn ensure_not_weaker(
    current: AuthMethod,
    target: AuthMethod,
    target_allows_sso: bool,
) -> Result<(), MethodError> {
    MethodGuard::may_switch(
        current,
        &TargetSignIn {
            method: target,
            allows_sso: target_allows_sso,
        },
    )
}

/// A shared port.
pub type SharedPolicy = Arc<dyn SigninPolicyPort>;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn policy(mode: SigninMode, break_glass: &[RecordId]) -> SigninPolicy {
        SigninPolicy {
            mode,
            break_glass: break_glass.to_vec(),
        }
    }

    #[test]
    fn the_policy_table() {
        let admin = RecordId::new_v7();
        let other = RecordId::new_v7();
        let bg = [admin];
        for mode in [SigninMode::Local, SigninMode::SsoOptional] {
            let p = policy(mode, &bg);
            assert_eq!(
                decide_password(Some(&p), Some(other)),
                PasswordDecision::Allow
            );
            assert_eq!(decide_password(Some(&p), None), PasswordDecision::Allow);
        }
        let required = policy(SigninMode::SsoRequired, &bg);
        assert_eq!(
            decide_password(Some(&required), Some(admin)),
            PasswordDecision::BreakGlass
        );
        assert_eq!(
            decide_password(Some(&required), Some(other)),
            PasswordDecision::Refuse
        );
        assert_eq!(
            decide_password(Some(&required), None),
            PasswordDecision::Refuse
        );
    }

    #[test]
    fn an_unreadable_policy_refuses_everyone_even_a_break_glass_administrator() {
        assert_eq!(
            decide_password(None, Some(RecordId::new_v7())),
            PasswordDecision::Refuse
        );
        assert_eq!(decide_password(None, None), PasswordDecision::Refuse);
    }

    #[test]
    fn a_session_never_moves_to_a_weaker_method() {
        assert!(ensure_not_weaker(AuthMethod::Sso, AuthMethod::Password, true).is_err());
        assert!(ensure_not_weaker(AuthMethod::Sso, AuthMethod::Sso, false).is_err());
        assert!(ensure_not_weaker(AuthMethod::Sso, AuthMethod::Hardware, true).is_ok());
        assert!(ensure_not_weaker(AuthMethod::Password, AuthMethod::Sso, true).is_ok());
        assert!(ensure_not_weaker(AuthMethod::PasswordTotp, AuthMethod::Password, true).is_err());
    }

    #[tokio::test]
    async fn the_default_port_allows_local_passwords() {
        let p = AllowLocal.policy("acme").await.unwrap();
        assert_eq!(p.mode, SigninMode::Local);
    }
}
