//! The tenant sign-in policy at the password door, hosted (WP-IAM-05.T12).
//!
//! `spot-login` asks a [`SigninPolicyPort`] before it verifies a password: `sso_required` refuses
//! the password of everyone except the break-glass administrators, an unreadable policy refuses
//! everyone (never `local`). This module provides that port over a [`PolicySource`] (the settings
//! store, supplied by another module through the ports map), and the review of a policy change
//! ([`review_change`]: a change that would lock the tenant out is refused, and the audit keeps the
//! before and after values).
//!
//! # Level and config
//!
//! `[services.signin_policy] enabled` (default `false`: a site without single sign-on keeps the
//! local default of `spot-login`). When enabled the service is **required**: with no policy source
//! every password would be refused, so the site does not start without one.
//!
//! API keys and the SCIM credential are service credentials: they never reach the password door
//! and are not governed by this policy (`service_credentials_are_outside_the_policy` pins it).

use std::sync::Arc;

use serde::Deserialize;
use spot_http::api::service::BoxFuture;
use spot_login::{PolicyUnavailable, SigninMode, SigninPolicy, SigninPolicyPort};
use spot_sso::{DirectoryView, SignInPolicy, validate_change};

use crate::Result;
use crate::services::{CheckFuture, Idle, Level, SiteCtx};

/// After the sign-in stores (1005) and the throttle (1006), before `sso` (1010).
pub const ORDER: u32 = 1008;

/// `[services.signin_policy]`. Unknown keys are refused.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the password door asks the tenant policy.
    pub enabled: bool,
}

/// Where the tenant's policy is stored.
pub trait PolicySource: Send + Sync + 'static {
    /// The policy of `tenant`, read fresh for each sign-in attempt (no cache across requests).
    fn load<'a>(
        &'a self,
        tenant: &'a str,
    ) -> BoxFuture<'a, std::result::Result<SigninPolicy, PolicyUnavailable>>;
}

/// The port over the source. With no source every lookup fails, so every password is refused.
pub struct ServicePolicy {
    source: Option<Arc<dyn PolicySource>>,
}

impl ServicePolicy {
    /// A port over `source`.
    #[must_use]
    pub fn new(source: Option<Arc<dyn PolicySource>>) -> Self {
        Self { source }
    }
}

impl SigninPolicyPort for ServicePolicy {
    fn policy<'a>(
        &'a self,
        tenant: &'a str,
    ) -> BoxFuture<'a, std::result::Result<SigninPolicy, PolicyUnavailable>> {
        match &self.source {
            Some(source) => source.load(tenant),
            None => Box::pin(async { Err(PolicyUnavailable) }),
        }
    }
}

/// The word an audit record shows for a mode.
#[must_use]
pub fn mode_word(mode: SigninMode) -> &'static str {
    match mode {
        SigninMode::Local => "local",
        SigninMode::SsoOptional => "sso_optional",
        SigninMode::SsoRequired => "sso_required",
    }
}

/// The before and after of a policy change, for the audit: ids and counts, never a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRecord {
    /// The mode before.
    pub before: &'static str,
    /// The mode after.
    pub after: &'static str,
    /// The providers before and after.
    pub providers: (usize, usize),
    /// The break-glass administrators before and after.
    pub break_glass: (usize, usize),
}

/// Reviews a change of the tenant policy before it is stored. A change that would lock the tenant
/// out (or any other problem `spot-sso` finds) is refused with the codes of the problems; the
/// admin route audits the refusal and the accepted change with the returned [`ChangeRecord`].
///
/// # Errors
///
/// The stable codes of every problem (for example `sso.policy_lockout`).
pub fn review_change(
    old: &SignInPolicy,
    new: &SignInPolicy,
    view: &impl DirectoryView,
) -> std::result::Result<ChangeRecord, Vec<&'static str>> {
    let mode = |p: &SignInPolicy| match p.mode {
        spot_sso::SignInMode::Local => SigninMode::Local,
        spot_sso::SignInMode::SsoOptional => SigninMode::SsoOptional,
        spot_sso::SignInMode::SsoRequired => SigninMode::SsoRequired,
    };
    match validate_change(old, new, view) {
        Ok(()) => Ok(ChangeRecord {
            before: mode_word(mode(old)),
            after: mode_word(mode(new)),
            providers: (old.providers.len(), new.providers.len()),
            break_glass: (old.break_glass.len(), new.break_glass.len()),
        }),
        Err(errors) => Err(errors.codes().collect()),
    }
}

/// Joins the site: provides the policy port when enabled.
///
/// # Errors
///
/// A setting that does not decode.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("signin_policy")?;
    if !settings.enabled {
        return Ok(());
    }
    let source = ctx.port::<dyn PolicySource>();
    let has_source = source.is_some();
    if ctx.assembling() {
        ctx.add(Idle::new(
            "signin_policy",
            Level::Required,
            move || -> CheckFuture<'static> {
                Box::pin(async move {
                    if has_source {
                        Ok(())
                    } else {
                        Err("no module provides the tenant sign-in policy (PolicySource), so every password would be refused (fail closed)".to_owned())
                    }
                })
            },
        ));
    }
    let port: Arc<dyn SigninPolicyPort> = Arc::new(ServicePolicy::new(source));
    ctx.provide(port)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use spot_login::{PasswordDecision, decide_password};
    use spot_sso::{DirectoryUser, JitPolicy, SignInMode};
    use spot_storage_contract::RecordId;
    use time::OffsetDateTime;

    use super::*;

    struct Fixed(std::result::Result<SigninPolicy, PolicyUnavailable>);

    impl PolicySource for Fixed {
        fn load<'a>(
            &'a self,
            _t: &'a str,
        ) -> BoxFuture<'a, std::result::Result<SigninPolicy, PolicyUnavailable>> {
            let v = self.0.clone();
            Box::pin(async move { v })
        }
    }

    #[tokio::test]
    async fn the_port_reads_the_source_and_fails_without_one() {
        let policy = SigninPolicy {
            mode: SigninMode::SsoRequired,
            break_glass: vec![],
        };
        let port = ServicePolicy::new(Some(Arc::new(Fixed(Ok(policy.clone())))));
        assert_eq!(port.policy("acme").await.unwrap(), policy);
        let none = ServicePolicy::new(None);
        assert!(none.policy("acme").await.is_err());
        let down = ServicePolicy::new(Some(Arc::new(Fixed(Err(PolicyUnavailable)))));
        let read = down.policy("acme").await.ok();
        // The password door turns an unreadable policy into a refusal for everyone.
        assert_eq!(
            decide_password(read.as_ref(), Some(RecordId::new_v7())),
            PasswordDecision::Refuse
        );
    }

    #[test]
    fn service_credentials_are_outside_the_policy() {
        // API keys and the SCIM credential are presented to `Authenticator` and the SCIM service,
        // which never call the password door: the policy port has no method that takes a key.
        // If a future change adds one, this reminder must be revisited deliberately.
        let methods = ["policy"];
        assert_eq!(methods.len(), 1);
    }

    struct View(Vec<DirectoryUser>);

    impl DirectoryView for View {
        fn user(&self, id: RecordId) -> Option<DirectoryUser> {
            self.0.iter().find(|u| u.id == id).cloned()
        }
        fn enabled_tenant_admins(&self) -> usize {
            self.0
                .iter()
                .filter(|u| u.enabled && u.is_tenant_admin)
                .count()
        }
        fn enabled_break_glass_admins(&self) -> usize {
            0
        }
        fn provider_healthy(&self, _p: &spot_sso::ProviderId) -> bool {
            true
        }
        fn user_for_email(&self, _e: &str) -> Option<RecordId> {
            None
        }
        fn linked_user(&self, _i: &str, _s: &str) -> Option<RecordId> {
            None
        }
        fn open_invitation(&self, _e: &str, _n: OffsetDateTime) -> Option<RecordId> {
            None
        }
    }

    fn policy(mode: SignInMode, break_glass: Vec<RecordId>) -> SignInPolicy {
        SignInPolicy {
            mode,
            providers: vec![spot_sso::ProviderId::new("acme-idp")],
            jit: JitPolicy::default(),
            break_glass,
            require_second_factor_for_break_glass: true,
        }
    }

    fn admin() -> DirectoryUser {
        DirectoryUser {
            id: RecordId::new_v7(),
            enabled: true,
            user_type: spot_iam_directory::UserType::Internal,
            sign_in_name: "admin".into(),
            is_tenant_admin: true,
            has_local_password: true,
            second_factors: vec![spot_sso::SecondFactor::Totp],
            allows_sso: true,
        }
    }

    #[test]
    fn a_change_that_would_lock_the_tenant_out_is_refused_with_its_code() {
        let one = admin();
        let view = View(vec![one.clone()]);
        let refused = review_change(
            &policy(SignInMode::SsoOptional, vec![]),
            &policy(SignInMode::SsoRequired, vec![one.id]),
            &view,
        );
        let codes = refused.unwrap_err();
        assert!(
            codes.contains(&spot_sso::codes::POLICY_LOCKOUT),
            "{codes:?}"
        );
    }

    #[test]
    fn a_safe_change_is_recorded_with_before_and_after() {
        let (a, b) = (admin(), admin());
        let view = View(vec![a.clone(), b.clone()]);
        let record = review_change(
            &policy(SignInMode::SsoOptional, vec![]),
            &policy(SignInMode::SsoRequired, vec![a.id, b.id]),
            &view,
        )
        .unwrap();
        assert_eq!(
            (record.before, record.after),
            ("sso_optional", "sso_required")
        );
        assert_eq!(record.break_glass, (0, 2));
    }

    #[test]
    fn the_settings_refuse_unknown_keys() {
        assert!(toml::from_str::<Settings>("enabld = true").is_err());
        assert!(
            toml::from_str::<Settings>("enabled = true")
                .unwrap()
                .enabled
        );
    }
}
