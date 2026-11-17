//! The tenant sign-in policy at the password door over the real stack (WP-IAM-05.T12).
//!
//! Blocked: the case flips the policy over the admin route (`sso_admin_policy_put`) and signs in
//! each way (password under `sso_required` refused with `sso.required`, a break-glass administrator
//! allowed, a third administrator refused, an unreadable policy refusing everyone). The site
//! cannot serve that yet: no module implements `SsoAdminPort` (the admin route answers as an
//! unserved operation) and no module provides a `PolicySource` for `services/signin_policy.rs`,
//! which is a required service that refuses to start without one. The decisions themselves are
//! pinned by the unit tests of `spot-login::policy` and `services/signin_policy.rs`.

use super::CaseDef;

/// After the single logout case.
pub const ORDER: u32 = 35;

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::blocked(
        "signin_policy: required_sso_refuses_passwords_except_break_glass_and_an_unreadable_policy_refuses_everyone",
        "Sign-in policy",
        "under `sso_required` a password sign-in is refused with `sso.required` except for the break-glass administrators, a third administrator is refused, and a policy that cannot be read refuses everyone",
        "WP-IAM-05.T10",
        "no module implements SsoAdminPort (the policy admin route is unserved) and none provides a PolicySource for the sign-in policy service",
    )]
}
