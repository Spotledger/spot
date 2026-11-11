//! The tenant sign-in policy: which methods a tenant allows and who may bypass SSO.
//!
//! The policy is `local`, `sso_optional` or `sso_required` (ADR-070 section 5.2). Under
//! `sso_required`, local sign-in is refused for everyone except the **break-glass
//! administrators**, of whom at least two must exist and each of whom must be a real,
//! enabled administrator with a local password and a second factor. Every break-glass use
//! raises an alert-level event, returned here as data for the caller to journal.
//!
//! A policy change that would lock the tenant out — switching to required with fewer than two
//! usable break-glass administrators — is refused (`validate_change`), problems listed.

use serde::{Deserialize, Serialize};
use spot_iam_directory::UserType;
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

use crate::error::{self as e, Result, ResultList, SsoError, SsoErrors};
use crate::provider::ProviderId;

/// How a principal proves who they are, where a second factor was used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecondFactor {
    /// A time-based one-time code.
    Totp,
    /// A hardware-bound authenticator.
    Hardware,
}

/// What the policy needs to know about a user. The directory supplies it; this crate assumes
/// nothing about how it is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryUser {
    /// The stable user id.
    pub id: RecordId,
    /// Whether the user may sign in at all.
    pub enabled: bool,
    /// The kind of principal.
    pub user_type: UserType,
    /// The sign-in name (an address or a username).
    pub sign_in_name: String,
    /// Whether the user is an enabled tenant administrator.
    pub is_tenant_admin: bool,
    /// Whether a local password is set.
    pub has_local_password: bool,
    /// The registered second factors.
    pub second_factors: Vec<SecondFactor>,
    /// Whether the user allows single sign-on at all.
    pub allows_sso: bool,
}

impl DirectoryUser {
    /// Whether the user has a TOTP or hardware second factor.
    #[must_use]
    pub fn has_strong_second_factor(&self) -> bool {
        self.second_factors
            .iter()
            .any(|f| matches!(f, SecondFactor::Totp | SecondFactor::Hardware))
    }
}

/// What the sign-in policy and the resolution need to read from the tenant directory.
pub trait DirectoryView {
    /// The user, when it exists.
    fn user(&self, id: RecordId) -> Option<DirectoryUser>;
    /// How many enabled tenant administrators exist.
    fn enabled_tenant_admins(&self) -> usize;
    /// How many enabled break-glass administrators exist.
    fn enabled_break_glass_admins(&self) -> usize;
    /// Whether a provider is configured and currently healthy.
    fn provider_healthy(&self, provider: &ProviderId) -> bool;
    /// The user whose sign-in name is `email`, when one exists (case-insensitive).
    fn user_for_email(&self, email: &str) -> Option<RecordId>;
    /// The user a `(issuer, subject)` linked identity belongs to.
    fn linked_user(&self, issuer: &str, subject: &str) -> Option<RecordId>;
    /// The open invitation for `email`, when one exists.
    fn open_invitation(&self, email: &str, now: OffsetDateTime) -> Option<RecordId>;
}

/// Which sign-in methods a tenant allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInMode {
    /// Local passwords only; SSO endpoints are refused.
    Local,
    /// Local passwords and SSO.
    SsoOptional,
    /// SSO only, except the break-glass administrators.
    SsoRequired,
}

/// When a tenant allows just-in-time creation at first SSO login (ADR-070 section 6.1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "rules")]
pub enum JitPolicy {
    /// Off: an unknown identity is denied (the default).
    #[default]
    Off,
    /// On, but only for identities a rule matches.
    RuleBased(Vec<JitRule>),
}

impl JitPolicy {
    /// The first rule matching `identity`, if just-in-time creation is enabled.
    #[must_use]
    pub fn matching_rule(&self, identity: &crate::SsoIdentity) -> Option<&JitRule> {
        match self {
            Self::Off => None,
            Self::RuleBased(rules) => rules.iter().find(|rule| rule.matches(identity)),
        }
    }
}

/// One just-in-time rule: an identity matching it may create a user of a given kind.
///
/// A created user gets **no roles** until a rule or an administrator grants one
/// (ADR-070 section 6.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JitRule {
    /// Matches the e-mail's domain, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_domain: Option<String>,
    /// Matches membership of this group, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The kind of user the rule creates.
    pub user_type: UserType,
}

impl JitRule {
    /// Whether `identity` satisfies every condition the rule sets.
    #[must_use]
    pub fn matches(&self, identity: &crate::SsoIdentity) -> bool {
        let domain_ok = match &self.email_domain {
            None => true,
            Some(want) => identity
                .email
                .as_deref()
                .and_then(email_domain)
                .is_some_and(|got| got.eq_ignore_ascii_case(want)),
        };
        let group_ok = match &self.group {
            None => true,
            Some(want) => identity.groups.iter().any(|got| got == want),
        };
        (self.email_domain.is_some() || self.group.is_some()) && domain_ok && group_ok
    }
}

/// The e-mail's domain part, lower-cased.
fn email_domain(email: &str) -> Option<&str> {
    email.split_once('@').map(|(_, domain)| domain)
}

/// The tenant's sign-in policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignInPolicy {
    /// Which methods the tenant allows.
    pub mode: SignInMode,
    /// The providers the tenant trusts.
    pub providers: Vec<ProviderId>,
    /// Just-in-time creation.
    #[serde(default)]
    pub jit: JitPolicy,
    /// The break-glass administrators.
    #[serde(default, with = "crate::wire::vec_record_id")]
    pub break_glass: Vec<RecordId>,
    /// Whether a break-glass sign-in requires a second factor.
    #[serde(default = "yes")]
    pub require_second_factor_for_break_glass: bool,
}

fn yes() -> bool {
    true
}

impl SignInPolicy {
    /// A local-only policy with no providers and no break-glass administrators.
    #[must_use]
    pub fn local() -> Self {
        Self {
            mode: SignInMode::Local,
            providers: Vec::new(),
            jit: JitPolicy::Off,
            break_glass: Vec::new(),
            require_second_factor_for_break_glass: true,
        }
    }
}

/// An alert-level event raised on every break-glass sign-in, as data for the caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakGlassAlert {
    /// The administrator who signed in.
    #[serde(with = "crate::wire::record_id")]
    pub user: RecordId,
    /// The fixed severity.
    pub level: AlertLevel,
    /// The fixed reason, for the audit trail.
    pub reason: BreakGlassReason,
}

/// The severity of a break-glass alert.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertLevel {
    /// An alert, not merely an information note.
    Alert,
}

/// Why a break-glass alert was raised.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakGlassReason {
    /// A break-glass administrator signed in with a local password while SSO is required.
    LocalSignInWhileSsoRequired,
}

/// The outcome of a local sign-in attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalDecision {
    /// Local sign-in is allowed.
    Allowed,
    /// Local sign-in is allowed as a break-glass administrator; the caller must journal the
    /// alert and, when [`LocalDecision::AllowedBreakGlass::second_factor_required`] is set,
    /// require a second factor.
    AllowedBreakGlass {
        /// Whether a second factor is required for this sign-in.
        second_factor_required: bool,
        /// The alert the caller must journal.
        alert: BreakGlassAlert,
    },
}

/// Checks a policy, reporting every problem at once.
///
/// While SSO is required, at least two break-glass administrators must exist, each a real,
/// enabled administrator with a local password and — when the policy requires it — a TOTP or
/// hardware second factor; and at least one configured provider must be healthy.
#[must_use]
pub fn check_policy(policy: &SignInPolicy, view: &impl DirectoryView) -> Vec<SsoError> {
    let mut problems = Vec::new();
    if policy.mode == SignInMode::SsoRequired {
        if policy.break_glass.len() < 2 || view.enabled_tenant_admins() < 2 {
            problems.push(SsoError::new(
                e::codes::BREAK_GLASS_TOO_FEW,
                "The tenant's sign-in policy is not safe.",
                "Appoint at least two break-glass administrators before requiring single sign-on.",
            ));
        }
        for user in &policy.break_glass {
            match view.user(*user) {
                None => problems.push(
                    SsoError::new(
                        e::codes::BREAK_GLASS_NOT_ADMIN,
                        "The tenant's sign-in policy is not safe.",
                        "Make every break-glass entry an enabled tenant administrator.",
                    )
                    .with_detail(user.to_string()),
                ),
                Some(directory_user) => {
                    if !directory_user.enabled || !directory_user.is_tenant_admin {
                        problems.push(
                            SsoError::new(
                                e::codes::BREAK_GLASS_NOT_ADMIN,
                                "The tenant's sign-in policy is not safe.",
                                "Make every break-glass entry an enabled tenant administrator.",
                            )
                            .with_detail(user.to_string()),
                        );
                    }
                    if !directory_user.has_local_password {
                        problems.push(
                            SsoError::new(
                                e::codes::BREAK_GLASS_NO_PASSWORD,
                                "The tenant's sign-in policy is not safe.",
                                "Give every break-glass administrator a local password.",
                            )
                            .with_detail(user.to_string()),
                        );
                    }
                    if policy.require_second_factor_for_break_glass
                        && !directory_user.has_strong_second_factor()
                    {
                        problems.push(
                            SsoError::new(
                                e::codes::BREAK_GLASS_NO_SECOND_FACTOR,
                                "The tenant's sign-in policy is not safe.",
                                "Register a TOTP or hardware second factor for every break-glass administrator.",
                            )
                            .with_detail(user.to_string()),
                        );
                    }
                }
            }
        }
        if !policy
            .providers
            .iter()
            .any(|provider| view.provider_healthy(provider))
        {
            problems.push(SsoError::new(
                e::codes::NO_PROVIDER,
                "The tenant's sign-in policy is not safe.",
                "Configure and test at least one identity provider before requiring single sign-on.",
            ));
        }
    }
    problems
}

/// Refuses a policy change that would lock the tenant out, reporting every problem.
///
/// # Errors
///
/// [`SsoErrors`] with the problems of the new policy, plus [`e::codes::POLICY_LOCKOUT`] when
/// the change moves to required without enough break-glass administrators.
pub fn validate_change(
    old: &SignInPolicy,
    new: &SignInPolicy,
    view: &impl DirectoryView,
) -> ResultList<()> {
    let mut problems = check_policy(new, view);
    if new.mode == SignInMode::SsoRequired && old.mode != SignInMode::SsoRequired {
        let usable = new
            .break_glass
            .iter()
            .filter(|id| {
                view.user(**id)
                    .is_some_and(|u| u.enabled && u.is_tenant_admin && u.has_local_password)
            })
            .count();
        if usable < 2 {
            problems.push(SsoError::new(
                e::codes::POLICY_LOCKOUT,
                "This change would lock everyone out of the tenant.",
                "Keep local sign-in until two break-glass administrators can sign in with a second factor.",
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(SsoErrors::new(problems))
    }
}

/// Decides whether `user` may sign in locally.
///
/// # Errors
///
/// [`e::codes::REQUIRED`] under `sso_required` for anyone but a break-glass administrator;
/// [`e::codes::UNKNOWN_USER`] for an id the directory does not know.
pub fn decide_local_signin(
    policy: &SignInPolicy,
    view: &impl DirectoryView,
    user: RecordId,
) -> Result<LocalDecision> {
    let directory_user = view.user(user).ok_or_else(|| {
        SsoError::new(
            e::codes::UNKNOWN_USER,
            "That account is not known.",
            "Check the user exists and is enabled.",
        )
    })?;
    if policy.mode != SignInMode::SsoRequired {
        return Ok(LocalDecision::Allowed);
    }
    let is_break_glass = policy.break_glass.contains(&user);
    if is_break_glass && directory_user.enabled && directory_user.is_tenant_admin {
        return Ok(LocalDecision::AllowedBreakGlass {
            second_factor_required: policy.require_second_factor_for_break_glass,
            alert: BreakGlassAlert {
                user,
                level: AlertLevel::Alert,
                reason: BreakGlassReason::LocalSignInWhileSsoRequired,
            },
        });
    }
    Err(SsoError::new(
        e::codes::REQUIRED,
        "Local sign-in is not available for this tenant.",
        "Sign in with single sign-on, or use a break-glass administrator account.",
    ))
}

/// Whether single sign-on with `provider` is allowed at all under `policy`.
///
/// # Errors
///
/// [`e::codes::SSO_NOT_ALLOWED`] under a local-only policy; [`e::codes::PROVIDER_NOT_CONFIGURED`]
/// for a provider the policy does not trust.
pub fn decide_sso_signin(policy: &SignInPolicy, provider: &ProviderId) -> Result<()> {
    if policy.mode == SignInMode::Local {
        return Err(SsoError::new(
            e::codes::SSO_NOT_ALLOWED,
            "Single sign-on is not available for this tenant.",
            "Enable single sign-on in the tenant's sign-in policy first.",
        ));
    }
    if !policy.providers.contains(provider) {
        return Err(SsoError::new(
            e::codes::PROVIDER_NOT_CONFIGURED,
            "That sign-in method is not available.",
            "Trust the provider in the tenant's sign-in policy first.",
        )
        .with_detail(provider.as_str()));
    }
    Ok(())
}
