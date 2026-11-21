//! The identity administration seam of `/api/v1/identity/*` (contract 0.18.0, WP-CON-03.T41,
//! WP-IAM-12.T1): users, invitations, roles, assignments, assignment rules, API keys, SCIM
//! credentials, administrative session control and support impersonation.
//!
//! `spot-http` parses, bounds, authenticates and renders; every decision belongs to the host's
//! adapters. Four ports carry the group, and the group is mounted only when all four are present:
//!
//! - [`IdentityAdminPort`]: the operations, in five small area traits ([`UserAdmin`],
//!   [`RoleAdmin`], [`AssignmentAdmin`], [`KeyAdmin`], [`SessionAdmin`]) the host fills in turns.
//!   An area the host did not fill answers `501 service.not_configured`.
//! - [`IdentityPermissionPort`]: who the caller is and whether the platform permission is held.
//!   It fails closed: any error, a missing role snapshot or a timeout is a refusal.
//! - [`IdentityStepUpPort`]: whether the caller's step-up is recent (`auth_step_up`).
//! - [`IdentityAuditPort`]: the audit record of every refusal and failure the layer produces. A
//!   success is audited by the adapter that performed the change, in the change's transaction.
//!
//! No input names an actor, an approver or a tenant: the acting user comes from the credential
//! only, and the tenant is the site. Types here mirror the contract's wire shapes; the directory
//! crate is not a dependency. A secret exists only on [`NewApiKey`] and [`NewScimCredential`].

use serde_json::{Map, Value as Json, json};

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`IdentityAdminPort`].
pub const KEY: PortKey = PortKey::of::<dyn IdentityAdminPort>();

/// What a port call answers.
pub type IdResult<T> = Result<T, ApiError>;

/// The platform permissions of the group (contract 0.18.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityPermission {
    /// `identity.read`.
    Read,
    /// `identity.manage`.
    Manage,
    /// `identity.roles.manage`.
    RolesManage,
    /// `identity.assign`.
    Assign,
    /// `identity.keys.manage`.
    KeysManage,
    /// `identity.impersonate`: the support tier, never held by a tenant role.
    Impersonate,
    /// The support person who started the running impersonation (ending it).
    ImpersonationOwner,
}

impl IdentityPermission {
    /// The permission's name as the contract writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityPermission::Read => "identity.read",
            IdentityPermission::Manage => "identity.manage",
            IdentityPermission::RolesManage => "identity.roles.manage",
            IdentityPermission::Assign => "identity.assign",
            IdentityPermission::KeysManage => "identity.keys.manage",
            IdentityPermission::Impersonate => "identity.impersonate",
            IdentityPermission::ImpersonationOwner => "identity.impersonate.own",
        }
    }
}

/// The audience of a signed-in caller (ADR-083).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallerAudience {
    /// A member of staff.
    Staff,
    /// A partner user.
    Partner,
    /// A consumer: never allowed anywhere in this group.
    Consumer,
}

/// The caller the permission port resolved from the credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityCaller {
    /// The caller's user id (a record id).
    pub user_id: String,
    /// The caller's audience.
    pub audience: CallerAudience,
}

/// The permission seam. It resolves the caller from `ctx.credential` on `ctx.site` and answers
/// whether `permission` is held on the server's role snapshot. Any failure to decide is an `Err`
/// (the layer turns every `Err` into a refusal, never an allow).
pub trait IdentityPermissionPort: Send + Sync {
    /// The caller, when they hold `permission`; `Err` otherwise.
    fn check<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        permission: IdentityPermission,
    ) -> BoxFuture<'a, IdResult<IdentityCaller>>;
}

/// The step-up seam.
pub trait IdentityStepUpPort: Send + Sync {
    /// Whether the caller's step-up is recent enough. `Err` is treated as "not recent".
    fn is_recent<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<bool>>;
}

/// What an audited operation ended as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditOutcome {
    /// Refused before the port was called (audience, permission, step-up, bad input).
    Refused,
    /// The port was called and answered an error.
    Failed,
}

/// One audit record of the layer. It names the operation, the outcome, the stable error code and
/// the validated target id; it never carries a body, a secret or an e-mail address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// The contract operation id.
    pub operation: &'static str,
    /// The outcome.
    pub outcome: AuditOutcome,
    /// The stable error code answered.
    pub code: String,
    /// The caller's user id when it was resolved.
    pub actor: Option<String>,
    /// The id the request addressed, when it is a valid record id.
    pub target: Option<String>,
}

/// The audit seam.
pub trait IdentityAuditPort: Send + Sync {
    /// Writes the record. Best effort for the layer: a failure is logged and never changes the
    /// answer already decided.
    fn record<'a>(&'a self, ctx: &'a RequestCtx, entry: AuditEntry) -> BoxFuture<'a, IdResult<()>>;
}

// ---------------------------------------------------------------------------
// Shared input types

/// A page request: `limit` 1 to 100 (the layer caps it) and an opaque cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageIn {
    /// The `next_cursor` of the previous page, shape-checked.
    pub cursor: Option<String>,
    /// The page size, 1 to 100.
    pub limit: usize,
}

/// A page answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    /// The items (at most the request's limit).
    pub items: Vec<T>,
    /// The cursor of the next page, `None` at the end.
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    /// The contract's `{data, next_cursor}` envelope.
    pub fn json(&self, one: impl Fn(&T) -> Json) -> Json {
        json!({
            "data": self.items.iter().map(one).collect::<Vec<_>>(),
            "next_cursor": self.next_cursor,
        })
    }
}

/// A user list request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserListIn {
    /// `invited`, `active`, `disabled` or `anonymised`.
    pub status: Option<String>,
    /// A name or e-mail prefix, at most 100 characters.
    pub q: Option<String>,
    /// The page.
    pub page: PageIn,
}

/// `identity_user_create`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserCreateIn {
    /// The e-mail address (at most 320 characters).
    pub email: String,
    /// The full name (1 to 200 characters).
    pub full_name: String,
    /// `staff` or `partner`.
    pub audience: String,
    /// Role names (at most 20).
    pub roles: Vec<String>,
    /// Invite (default) or create active without a password.
    pub invite: bool,
}

/// `identity_user_patch`: a merge patch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserPatchIn {
    /// The new full name.
    pub full_name: Option<String>,
    /// The new audience (`staff` or `partner`).
    pub audience: Option<String>,
}

/// `identity_assignment_list` filters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentListIn {
    /// Only this user's assignments.
    pub user: Option<String>,
    /// Only this role's assignments.
    pub role: Option<String>,
    /// The page.
    pub page: PageIn,
}

/// `identity_assignment_create`. The grantor is the caller; the body cannot name one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentCreateIn {
    /// The user receiving the role.
    pub user_id: String,
    /// The role.
    pub role_id: String,
    /// The scope, at most 200 characters.
    pub scope: Option<String>,
    /// RFC 3339 expiry.
    pub expires_at: Option<String>,
    /// The reason, at most 500 characters.
    pub reason: Option<String>,
}

/// One automatic assignment rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleIn {
    /// The rule id, at most 64 characters.
    pub id: String,
    /// The condition (expression text), at most 1000 characters.
    pub when: String,
    /// The role granted.
    pub role_id: String,
    /// The scope, at most 200 characters.
    pub scope: Option<String>,
    /// Whether the rule runs.
    pub enabled: bool,
}

/// `identity_api_key_create`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeyCreateIn {
    /// The key's name (1 to 100 characters).
    pub name: String,
    /// The owner user.
    pub owner_id: String,
    /// The scopes (at most 100, each at most 200 characters).
    pub scopes: Vec<String>,
    /// RFC 3339 expiry.
    pub expires_at: Option<String>,
}

/// `identity_session_list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionListIn {
    /// The user whose sessions are listed.
    pub user: String,
    /// The page.
    pub page: PageIn,
}

/// `identity_impersonation_start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImpersonationIn {
    /// The user to act as.
    pub user_id: String,
    /// Why (1 to 500 characters).
    pub reason: String,
    /// A ticket reference, at most 100 characters.
    pub ticket: Option<String>,
}

// ---------------------------------------------------------------------------
// Output types (the contract's wire shapes)

fn put(map: &mut Map<String, Json>, key: &str, value: Json) {
    map.insert(key.to_owned(), value);
}

fn put_opt(map: &mut Map<String, Json>, key: &str, value: &Option<String>) {
    if let Some(v) = value {
        map.insert(key.to_owned(), Json::from(v.clone()));
    }
}

fn put_null(map: &mut Map<String, Json>, key: &str, value: &Option<String>) {
    map.insert(key.to_owned(), value.clone().map_or(Json::Null, Json::from));
}

/// The contract's `IdentityUser`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserOut {
    /// Record id.
    pub id: String,
    /// E-mail address.
    pub email: String,
    /// Full name.
    pub full_name: String,
    /// `staff` or `partner`.
    pub audience: String,
    /// `invited`, `active`, `disabled` or `anonymised`.
    pub status: String,
    /// Role names.
    pub roles: Vec<String>,
    /// Whether a second factor is enrolled.
    pub second_factor: bool,
    /// Last sign-in.
    pub last_sign_in_at: Option<String>,
    /// Creation instant.
    pub created_at: String,
    /// Version (the `ETag`).
    pub version: u64,
}

impl UserOut {
    /// The wire object.
    pub fn json(&self) -> Json {
        let mut m = Map::new();
        put(&mut m, "id", self.id.clone().into());
        put(&mut m, "email", self.email.clone().into());
        put(&mut m, "full_name", self.full_name.clone().into());
        put(&mut m, "audience", self.audience.clone().into());
        put(&mut m, "status", self.status.clone().into());
        put(&mut m, "roles", json!(self.roles));
        put(&mut m, "second_factor", self.second_factor.into());
        put_null(&mut m, "last_sign_in_at", &self.last_sign_in_at);
        put(&mut m, "created_at", self.created_at.clone().into());
        put(&mut m, "version", self.version.into());
        Json::Object(m)
    }
}

/// What the leaver actions of a disable or enable did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserChangeOut {
    /// The user after the change.
    pub user: UserOut,
    /// `(action, count)` pairs.
    pub leaver_actions: Vec<(String, u64)>,
}

impl UserChangeOut {
    /// The wire object (`data`).
    pub fn json(&self) -> Json {
        let actions: Vec<Json> = self
            .leaver_actions
            .iter()
            .map(|(a, c)| json!({"action": a, "count": c}))
            .collect();
        json!({"user": self.user.json(), "leaver_actions": actions})
    }
}

/// How a recovery request ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoverOut {
    /// Sent (`202`).
    Sent,
    /// Needs a second approver (`202`); the request is decided through governance.
    NeedsApproval,
}

/// The contract's `IdentityInvitation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvitationOut {
    /// Record id.
    pub id: String,
    /// The invited user.
    pub user_id: String,
    /// The address invited.
    pub email: String,
    /// `pending`, `accepted`, `expired` or `revoked`.
    pub status: String,
    /// Expiry.
    pub expires_at: String,
    /// Sent at.
    pub sent_at: String,
}

impl InvitationOut {
    /// The wire object. The token is never part of it.
    pub fn json(&self) -> Json {
        json!({"id": self.id, "user_id": self.user_id, "email": self.email,
               "status": self.status, "expires_at": self.expires_at, "sent_at": self.sent_at})
    }
}

/// The contract's `IdentityRole`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleOut {
    /// Record id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Description.
    pub description: Option<String>,
    /// Built in (never edited).
    pub builtin: bool,
    /// The built-in role a copy came from.
    pub customised_from: Option<String>,
    /// Permission names.
    pub permissions: Vec<String>,
    /// Version.
    pub version: u64,
}

impl RoleOut {
    /// The wire object.
    pub fn json(&self) -> Json {
        let mut m = Map::new();
        put(&mut m, "id", self.id.clone().into());
        put(&mut m, "name", self.name.clone().into());
        put_opt(&mut m, "description", &self.description);
        put(&mut m, "builtin", self.builtin.into());
        put_opt(&mut m, "customised_from", &self.customised_from);
        put(&mut m, "permissions", json!(self.permissions));
        put(&mut m, "version", self.version.into());
        Json::Object(m)
    }
}

/// `identity_role_upgrade_diff`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeDiffOut {
    /// Permissions the upgrade would add.
    pub added: Vec<String>,
    /// Permissions the upgrade would remove.
    pub removed: Vec<String>,
    /// The version the copy came from.
    pub from_version: u64,
    /// The version the upgrade would reach.
    pub to_version: u64,
}

impl UpgradeDiffOut {
    /// The wire object (`data`).
    pub fn json(&self) -> Json {
        json!({"added": self.added, "removed": self.removed,
               "from_version": self.from_version, "to_version": self.to_version})
    }
}

/// The contract's `IdentityAssignment`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentOut {
    /// Record id.
    pub id: String,
    /// The user.
    pub user_id: String,
    /// The role.
    pub role_id: String,
    /// The scope.
    pub scope: Option<String>,
    /// Who granted it.
    pub granted_by: String,
    /// When.
    pub granted_at: String,
    /// Expiry.
    pub expires_at: Option<String>,
    /// `direct` or `rule`.
    pub source: Option<String>,
}

impl AssignmentOut {
    /// The wire object.
    pub fn json(&self) -> Json {
        let mut m = Map::new();
        put(&mut m, "id", self.id.clone().into());
        put(&mut m, "user_id", self.user_id.clone().into());
        put(&mut m, "role_id", self.role_id.clone().into());
        put_null(&mut m, "scope", &self.scope);
        put(&mut m, "granted_by", self.granted_by.clone().into());
        put(&mut m, "granted_at", self.granted_at.clone().into());
        put_null(&mut m, "expires_at", &self.expires_at);
        put_opt(&mut m, "source", &self.source);
        Json::Object(m)
    }
}

/// How a grant ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantOut {
    /// Granted (`201`); the assignment.
    Granted(AssignmentOut),
    /// Approval required (`202`): the grant request id.
    Requested(String),
}

/// The rule set with its version (the `ETag`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RulesOut {
    /// The rules.
    pub rules: Vec<RuleIn>,
    /// Version.
    pub version: u64,
}

impl RulesOut {
    /// The wire object (`data`).
    pub fn json(&self) -> Json {
        let rules: Vec<Json> = self
            .rules
            .iter()
            .map(|r| {
                json!({"id": r.id, "when": r.when, "role_id": r.role_id,
                       "scope": r.scope, "enabled": r.enabled})
            })
            .collect();
        json!({"rules": rules, "version": self.version})
    }
}

/// The plan a preview returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanOut {
    /// `(user_id, role_id, scope)` grants.
    pub grants: Vec<(String, String, Option<String>)>,
    /// Assignment ids to revoke.
    pub revokes: Vec<String>,
    /// More than the page's bound.
    pub truncated: bool,
    /// The digest `identity_assignment_rules_run` must repeat.
    pub plan_digest: String,
}

impl PlanOut {
    /// The wire object (`data`).
    pub fn json(&self) -> Json {
        let grants: Vec<Json> = self
            .grants
            .iter()
            .take(1000)
            .map(|(u, r, s)| json!({"user_id": u, "role_id": r, "scope": s}))
            .collect();
        let revokes: Vec<Json> = self
            .revokes
            .iter()
            .take(1000)
            .map(|a| json!({"assignment_id": a}))
            .collect();
        json!({"grants": grants, "revokes": revokes,
               "truncated": self.truncated || self.grants.len() > 1000 || self.revokes.len() > 1000,
               "plan_digest": self.plan_digest})
    }
}

/// The counts of a rules run (never facts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunOut {
    /// Grants made.
    pub granted: u64,
    /// Revokes made.
    pub revoked: u64,
    /// Changes the governance gate refused.
    pub refused: u64,
}

/// The contract's `IdentityApiKey` (no secret).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeyOut {
    /// Record id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Owner user.
    pub owner_id: String,
    /// Scopes.
    pub scopes: Vec<String>,
    /// Created.
    pub created_at: String,
    /// Last used.
    pub last_used_at: Option<String>,
    /// Expiry.
    pub expires_at: Option<String>,
    /// A short non-secret prefix.
    pub prefix: Option<String>,
}

impl ApiKeyOut {
    /// The wire object.
    pub fn json(&self) -> Json {
        let mut m = Map::new();
        put(&mut m, "id", self.id.clone().into());
        put(&mut m, "name", self.name.clone().into());
        put(&mut m, "owner_id", self.owner_id.clone().into());
        put(&mut m, "scopes", json!(self.scopes));
        put(&mut m, "created_at", self.created_at.clone().into());
        put_null(&mut m, "last_used_at", &self.last_used_at);
        put_null(&mut m, "expires_at", &self.expires_at);
        put_opt(&mut m, "prefix", &self.prefix);
        Json::Object(m)
    }
}

/// A newly created API key: the only type with a secret, shown once.
#[derive(Clone, PartialEq, Eq)]
pub struct NewApiKey {
    /// The key's record.
    pub key: ApiKeyOut,
    /// The secret (at most 256 characters).
    pub secret: String,
}

impl std::fmt::Debug for NewApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewApiKey")
            .field("key", &self.key)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl NewApiKey {
    /// The wire object (`data`).
    pub fn json(&self) -> Json {
        json!({"key": self.key.json(), "secret": self.secret})
    }
}

/// The contract's `IdentityScimCredential` (no secret).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScimOut {
    /// Record id.
    pub id: String,
    /// Created.
    pub created_at: String,
    /// Last used.
    pub last_used_at: Option<String>,
    /// A short non-secret prefix.
    pub prefix: Option<String>,
}

impl ScimOut {
    /// The wire object.
    pub fn json(&self) -> Json {
        let mut m = Map::new();
        put(&mut m, "id", self.id.clone().into());
        put(&mut m, "created_at", self.created_at.clone().into());
        put_null(&mut m, "last_used_at", &self.last_used_at);
        put_opt(&mut m, "prefix", &self.prefix);
        Json::Object(m)
    }
}

/// A newly created SCIM credential: a secret shown once.
#[derive(Clone, PartialEq, Eq)]
pub struct NewScimCredential {
    /// The credential's record.
    pub credential: ScimOut,
    /// The secret (at most 256 characters).
    pub secret: String,
}

impl std::fmt::Debug for NewScimCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewScimCredential")
            .field("credential", &self.credential)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl NewScimCredential {
    /// The wire object (`data`).
    pub fn json(&self) -> Json {
        json!({"credential": self.credential.json(), "secret": self.secret})
    }
}

/// The contract's `IdentitySession` (no token, no digest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminSessionOut {
    /// Session id.
    pub id: String,
    /// The user.
    pub user_id: String,
    /// Created.
    pub created_at: String,
    /// Last seen.
    pub last_seen_at: Option<String>,
    /// The client label.
    pub client: Option<String>,
    /// `password`, `sso` or `api_key`.
    pub method: Option<String>,
    /// The support person acting as this user.
    pub impersonated_by: Option<String>,
}

impl AdminSessionOut {
    /// The wire object.
    pub fn json(&self) -> Json {
        let mut m = Map::new();
        put(&mut m, "id", self.id.clone().into());
        put(&mut m, "user_id", self.user_id.clone().into());
        put(&mut m, "created_at", self.created_at.clone().into());
        put_opt(&mut m, "last_seen_at", &self.last_seen_at);
        put_opt(&mut m, "client", &self.client);
        put_opt(&mut m, "method", &self.method);
        put_opt(&mut m, "impersonated_by", &self.impersonated_by);
        Json::Object(m)
    }
}

// ---------------------------------------------------------------------------
// The area traits

/// Users and invitations. The adapter decides tenant scope, the last-administrator rule and
/// every refusal; a cross-tenant id answers `identity.not_found` exactly like a missing one.
pub trait UserAdmin: Send + Sync {
    /// `identity_user_list`.
    fn user_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        q: UserListIn,
    ) -> BoxFuture<'a, IdResult<Page<UserOut>>>;
    /// `identity_user_create`; `ctx.idempotency_key` is the command id.
    fn user_create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        input: UserCreateIn,
    ) -> BoxFuture<'a, IdResult<UserOut>>;
    /// `identity_user_get`.
    fn user_get<'a>(&'a self, ctx: &'a RequestCtx, id: &'a str)
    -> BoxFuture<'a, IdResult<UserOut>>;
    /// `identity_user_patch` with the `If-Match` version.
    fn user_patch<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        version: u64,
        patch: UserPatchIn,
    ) -> BoxFuture<'a, IdResult<UserOut>>;
    /// `identity_user_disable` (ends sessions and keys).
    fn user_disable<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<UserChangeOut>>;
    /// `identity_user_enable`.
    fn user_enable<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<UserChangeOut>>;
    /// `identity_user_anonymise`; `confirm_email` must equal the user's address.
    fn user_anonymise<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        confirm_email: String,
    ) -> BoxFuture<'a, IdResult<UserOut>>;
    /// `identity_user_recover` (`password_reset` or `second_factor_reset`).
    fn user_recover<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        kind: String,
    ) -> BoxFuture<'a, IdResult<RecoverOut>>;
    /// `identity_invitation_list`.
    fn invitation_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: PageIn,
    ) -> BoxFuture<'a, IdResult<Page<InvitationOut>>>;
    /// `identity_invitation_revoke` (a repeat is a no-op).
    fn invitation_revoke<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<()>>;
    /// `identity_invitation_resend` (the new token goes by mail only).
    fn invitation_resend<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<()>>;
}

/// Roles.
pub trait RoleAdmin: Send + Sync {
    /// `identity_role_list`.
    fn role_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: PageIn,
    ) -> BoxFuture<'a, IdResult<Page<RoleOut>>>;
    /// `identity_role_get`.
    fn role_get<'a>(&'a self, ctx: &'a RequestCtx, id: &'a str)
    -> BoxFuture<'a, IdResult<RoleOut>>;
    /// `identity_role_customise`: a copy that remembers its origin.
    fn role_customise<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        name: String,
    ) -> BoxFuture<'a, IdResult<RoleOut>>;
    /// `identity_role_upgrade_diff`: read only.
    fn role_upgrade_diff<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<UpgradeDiffOut>>;
}

/// Assignments and assignment rules.
pub trait AssignmentAdmin: Send + Sync {
    /// `identity_assignment_list`.
    fn assignment_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        q: AssignmentListIn,
    ) -> BoxFuture<'a, IdResult<Page<AssignmentOut>>>;
    /// `identity_assignment_create`: the grantor is the caller; the SoD gate and self-grant
    /// refusal are the adapter's.
    fn assignment_create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        input: AssignmentCreateIn,
    ) -> BoxFuture<'a, IdResult<GrantOut>>;
    /// `identity_assignment_delete` (a repeat is a no-op).
    fn assignment_delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<()>>;
    /// `identity_assignment_rules_get`.
    fn rules_get<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<RulesOut>>;
    /// `identity_assignment_rules_put` with the `If-Match` version.
    fn rules_put<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        version: u64,
        rules: Vec<RuleIn>,
    ) -> BoxFuture<'a, IdResult<RulesOut>>;
    /// `identity_assignment_rules_preview`: a dry run, applies nothing.
    fn rules_preview<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        rules: Option<Vec<RuleIn>>,
    ) -> BoxFuture<'a, IdResult<PlanOut>>;
    /// `identity_assignment_rules_run`: applies exactly the plan with this digest.
    fn rules_run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        plan_digest: String,
    ) -> BoxFuture<'a, IdResult<RunOut>>;
}

/// API keys and SCIM credentials.
pub trait KeyAdmin: Send + Sync {
    /// `identity_api_key_list`.
    fn api_key_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: PageIn,
    ) -> BoxFuture<'a, IdResult<Page<ApiKeyOut>>>;
    /// `identity_api_key_create`: the scope must be a subset of the owner's rights.
    fn api_key_create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        input: ApiKeyCreateIn,
    ) -> BoxFuture<'a, IdResult<NewApiKey>>;
    /// `identity_api_key_delete` (a repeat is a no-op).
    fn api_key_delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<()>>;
    /// `identity_scim_credential_list`.
    fn scim_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: PageIn,
    ) -> BoxFuture<'a, IdResult<Page<ScimOut>>>;
    /// `identity_scim_credential_create` (at most two per tenant).
    fn scim_create<'a>(&'a self, ctx: &'a RequestCtx)
    -> BoxFuture<'a, IdResult<NewScimCredential>>;
    /// `identity_scim_credential_delete` (a repeat is a no-op).
    fn scim_delete<'a>(&'a self, ctx: &'a RequestCtx, id: &'a str) -> BoxFuture<'a, IdResult<()>>;
}

/// Administrative session control and support impersonation.
pub trait SessionAdmin: Send + Sync {
    /// `identity_session_list`.
    fn session_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        q: SessionListIn,
    ) -> BoxFuture<'a, IdResult<Page<AdminSessionOut>>>;
    /// `identity_session_end` (a repeat is a no-op).
    fn session_end<'a>(&'a self, ctx: &'a RequestCtx, id: &'a str) -> BoxFuture<'a, IdResult<()>>;
    /// `identity_user_sessions_end_all`: how many were ended.
    fn sessions_end_all<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        user: &'a str,
    ) -> BoxFuture<'a, IdResult<u64>>;
    /// `identity_impersonation_start`: the expiry instant (RFC 3339).
    fn impersonation_start<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        input: ImpersonationIn,
    ) -> BoxFuture<'a, IdResult<String>>;
    /// `identity_impersonation_end` (a repeat is a no-op).
    fn impersonation_end<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<()>>;
}

/// The identity administration port: five areas, each optional. An area left `None` answers
/// `501 service.not_configured`.
pub trait IdentityAdminPort: Send + Sync {
    /// Users and invitations.
    fn users(&self) -> Option<&dyn UserAdmin> {
        None
    }
    /// Roles.
    fn roles(&self) -> Option<&dyn RoleAdmin> {
        None
    }
    /// Assignments and rules.
    fn assignments(&self) -> Option<&dyn AssignmentAdmin> {
        None
    }
    /// API keys and SCIM credentials.
    fn keys(&self) -> Option<&dyn KeyAdmin> {
        None
    }
    /// Sessions and impersonation.
    fn sessions(&self) -> Option<&dyn SessionAdmin> {
        None
    }
}
