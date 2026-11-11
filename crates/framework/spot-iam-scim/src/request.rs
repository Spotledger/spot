//! The request handlers: `handle(req, ctx) -> ScimResponse` (ADR-070 7.2).
//!
//! This is the whole SCIM 2.0 surface the platform serves: `/Users`, `/Groups`,
//! `/ServiceProviderConfig`, `/Schemas` and `/ResourceTypes`, with `GET`, `POST`, `PUT`,
//! `PATCH` and `DELETE`. It is sans-I/O: it validates the bearer credential, enforces the
//! request-size and rate limits, works through [`ScimStore`] and returns a [`ScimResponse`].
//! The HTTP adapter ([`crate::http`]) turns that into an axum response; the conformance tests
//! call it directly.
//!
//! # Deactivation, never deletion
//!
//! `PATCH active=false` (or a `PUT` that sets it) calls the directory's leaver path
//! ([`spot_iam_directory::User::disable`]) and carries out every [`LeaverAction`] it returns,
//! so the response's [`LeaverOutcome`] lists the sessions, refresh tokens, API keys and
//! invitations that ended. `DELETE /Users/{id}` is **not** deletion either: it maps to the same
//! deactivation and never removes the record. Only a `Group` may be deleted.

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
use spot_events_contract::Actor;
use spot_iam_access::KeyScope;
use spot_iam_assign::plan_idp;
use spot_iam_directory::{LeaverAction, User, UserType};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

use crate::TenantId;
use crate::error::{ScimError, ScimResult};
use crate::filter::{Filter, Target};
use crate::mapping::{GroupMapping, build_group_role_map};
use crate::patch::PatchRequest;
use crate::resource::{
    LIST_SCHEMA, Member, SCIM_BASE, ScimGroup, ScimUser, StoredGroup, StoredUser, USER_SCHEMA,
    etag, parse_etag, parse_record_id,
};
use crate::store::{AuditEntry, ScimEvent, ScimStore};

/// The HTTP method of a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
}

impl Method {
    /// Parses a method name (case-insensitive).
    pub fn parse(name: &str) -> Option<Method> {
        match name.to_ascii_uppercase().as_str() {
            "GET" => Some(Method::Get),
            "POST" => Some(Method::Post),
            "PUT" => Some(Method::Put),
            "PATCH" => Some(Method::Patch),
            "DELETE" => Some(Method::Delete),
            _ => None,
        }
    }

    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

/// The dedicated bearer credential SCIM is served with: an API key owned by a `service` user
/// (ADR-070 7.2, and the API-key rules of `spot-iam-access`).
#[derive(Clone, Debug)]
pub struct Credential {
    /// The key's id.
    pub key: RecordId,
    /// The service user that owns the key.
    pub owner: RecordId,
    /// The owner's kind; a SCIM credential is owned by a service (or internal) user.
    pub owner_type: UserType,
    /// Whether the owner is enabled now. Read from the owner's state on every use, never cached.
    pub owner_enabled: bool,
    /// The tenant the credential is for. No SCIM call reaches another tenant's records.
    pub tenant: TenantId,
    /// The key's scope, a subset of the owner's rights.
    pub scope: KeyScope,
    /// When the credential stops working, if it does.
    pub expires_at: Option<OffsetDateTime>,
}

impl Credential {
    /// Whether the credential may be used at `now`, or why not.
    pub fn authorise(&self, now: OffsetDateTime) -> ScimResult<()> {
        if !matches!(self.owner_type, UserType::Service | UserType::Internal) {
            return Err(ScimError::unauthorised(
                "a SCIM credential is owned by a service user",
            ));
        }
        if !self.owner_enabled {
            return Err(ScimError::unauthorised(
                "the owner of the credential is disabled",
            ));
        }
        if let Some(expires) = self.expires_at
            && now >= expires
        {
            return Err(ScimError::unauthorised("the credential has expired"));
        }
        if let Some(secs) = self.scope.expires_at
            && now.unix_timestamp() >= secs
        {
            return Err(ScimError::unauthorised("the credential has expired"));
        }
        if self.scope.allow.is_empty() {
            return Err(ScimError::unauthorised("the credential allows nothing"));
        }
        Ok(())
    }

    /// The actor every event and audit entry records: the credential's owner.
    pub fn actor(&self) -> Actor {
        Actor::User {
            id: self.owner,
            via: None,
        }
    }
}

/// The request bounds: body size, filter length, page size and rate limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The largest body this endpoint reads.
    pub max_body_bytes: usize,
    /// The longest filter it parses.
    pub max_filter_chars: usize,
    /// The page size when the client asks for none.
    pub default_count: usize,
    /// The largest page this endpoint returns.
    pub max_count: usize,
    /// How many requests one credential may send in the rate window.
    pub requests_per_window: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
            max_filter_chars: 1024,
            default_count: 100,
            max_count: 1000,
            requests_per_window: 600,
        }
    }
}

/// Everything a handler needs that is not the request: the tenant, the credential, the instant,
/// the limits and the store.
pub struct ScimContext<'a> {
    /// The tenant the credential is for.
    pub tenant: TenantId,
    /// The bearer credential.
    pub credential: &'a Credential,
    /// The instant; never read from a clock here.
    pub now: OffsetDateTime,
    /// The request bounds.
    pub limits: &'a Limits,
    /// The persistence and event sink.
    pub store: &'a mut dyn ScimStore,
}

impl ScimContext<'_> {
    fn actor(&self) -> Actor {
        self.credential.actor()
    }

    fn audit(&mut self, action: &str, subject: &str, detail: &str) {
        let entry = AuditEntry {
            tenant: self.tenant.clone(),
            at: self.now,
            actor: self.actor(),
            action: action.to_owned(),
            subject: subject.to_owned(),
            detail: detail.to_owned(),
        };
        self.store.audit(entry);
    }
}

/// A SCIM request, decoded from the wire by the HTTP adapter or built directly in a test.
#[derive(Clone, Debug)]
pub struct ScimRequest {
    /// The method.
    pub method: Method,
    /// The path, with or without the `/scim/v2` base.
    pub path: String,
    /// The query parameters, decoded.
    pub query: BTreeMap<String, String>,
    /// The JSON body, when there is one.
    pub body: Option<Json>,
    /// The `If-Match` header value, when the client sent one.
    pub if_match: Option<String>,
    /// The declared body size in bytes.
    pub content_length: usize,
}

impl ScimRequest {
    /// A request with no body, no query and no `If-Match`.
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: BTreeMap::new(),
            body: None,
            if_match: None,
            content_length: 0,
        }
    }

    /// Sets the body and its declared size.
    #[must_use]
    pub fn with_body(mut self, body: Json) -> Self {
        self.content_length = serde_json::to_string(&body).map(|s| s.len()).unwrap_or(0);
        self.body = Some(body);
        self
    }

    /// Adds a query parameter.
    #[must_use]
    pub fn with_query(mut self, name: &str, value: impl Into<String>) -> Self {
        self.query.insert(name.to_owned(), value.into());
        self
    }

    /// Sets the `If-Match` value.
    #[must_use]
    pub fn with_if_match(mut self, value: impl Into<String>) -> Self {
        self.if_match = Some(value.into());
        self
    }
}

/// The result of a deactivation: exactly the [`LeaverAction`]s the directory asked for, carried
/// out, with the counts of what ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeaverOutcome {
    /// The user deactivated.
    pub user: Option<RecordId>,
    /// Sessions ended.
    pub sessions_ended: usize,
    /// Refresh tokens revoked.
    pub refresh_tokens_revoked: usize,
    /// API keys disabled.
    pub api_keys_disabled: usize,
    /// Invitations cancelled.
    pub invitations_cancelled: usize,
    /// Open tasks reassigned or flagged.
    pub tasks_reassigned: usize,
    /// Whether the audit action ran.
    pub audit_written: bool,
}

/// A SCIM response.
#[derive(Clone, Debug, PartialEq)]
pub struct ScimResponse {
    /// The HTTP status.
    pub status: u16,
    /// The JSON body, when there is one.
    pub body: Option<Json>,
    /// The resource ETag, when there is one.
    pub etag: Option<String>,
    /// The `Location` header of a created resource.
    pub location: Option<String>,
    /// The deactivation outcome, when the request deactivated a user.
    pub outcome: Option<LeaverOutcome>,
    /// The `Retry-After` seconds of a `429`.
    pub retry_after: Option<u64>,
}

impl ScimResponse {
    /// A `200` with a body.
    pub fn ok(body: Json) -> Self {
        Self::new(200, Some(body))
    }

    /// A `201` with a body, a `Location` and an `ETag`.
    pub fn created(body: Json, location: String, etag: String) -> Self {
        Self {
            location: Some(location),
            etag: Some(etag),
            ..Self::new(201, Some(body))
        }
    }

    /// A `204` with no body.
    pub fn no_content() -> Self {
        Self::new(204, None)
    }

    /// The RFC 7644 error document for a refusal.
    pub fn error(error: &ScimError) -> Self {
        let body = serde_json::to_value(error.body()).unwrap_or(Json::Null);
        Self::new(error.status, Some(body))
    }

    fn new(status: u16, body: Option<Json>) -> Self {
        Self {
            status,
            body,
            etag: None,
            location: None,
            outcome: None,
            retry_after: None,
        }
    }

    fn with_etag(mut self, value: Option<String>) -> Self {
        self.etag = value;
        self
    }
}

/// Handles one SCIM request. This is the entry point the HTTP adapter and the tests call.
pub fn handle(request: ScimRequest, ctx: &mut ScimContext<'_>) -> ScimResponse {
    if let Err(error) = ctx.credential.authorise(ctx.now) {
        return ScimResponse::error(&error);
    }
    if request.content_length > ctx.limits.max_body_bytes {
        return ScimResponse::error(&ScimError::too_large(format!(
            "the body is larger than {} bytes",
            ctx.limits.max_body_bytes
        )));
    }
    if let Err(retry) =
        ctx.store
            .rate_check(&ctx.credential.key, ctx.limits.requests_per_window, ctx.now)
    {
        let mut response = ScimResponse::error(&ScimError::rate_limited(
            "too many requests for this credential",
        ));
        response.retry_after = Some(retry);
        return response;
    }
    match dispatch(&request, ctx) {
        Ok(response) => response,
        Err(error) => ScimResponse::error(&error),
    }
}

fn segments(path: &str) -> Vec<&str> {
    let path = path.strip_prefix(SCIM_BASE).unwrap_or(path);
    path.split('/').filter(|s| !s.is_empty()).collect()
}

fn dispatch(request: &ScimRequest, ctx: &mut ScimContext<'_>) -> ScimResult<ScimResponse> {
    let parts = segments(&request.path);
    match parts.as_slice() {
        ["ServiceProviderConfig"] => {
            expect(request.method, &[Method::Get])?;
            Ok(ScimResponse::ok(service_provider_config(ctx.limits)))
        }
        ["Schemas"] => {
            expect(request.method, &[Method::Get])?;
            Ok(ScimResponse::ok(list_response(schemas(), 2, 1)))
        }
        ["Schemas", id] => {
            expect(request.method, &[Method::Get])?;
            let schema = schemas()
                .into_iter()
                .find(|s| s.get("id").and_then(Json::as_str) == Some(*id))
                .ok_or_else(|| ScimError::not_found("no such schema"))?;
            Ok(ScimResponse::ok(schema))
        }
        ["ResourceTypes"] => {
            expect(request.method, &[Method::Get])?;
            let types = resource_types();
            let total = types.len();
            Ok(ScimResponse::ok(list_response(types, total, 1)))
        }
        ["ResourceTypes", id] => {
            expect(request.method, &[Method::Get])?;
            let resource_type = resource_types()
                .into_iter()
                .find(|s| s.get("id").and_then(Json::as_str) == Some(*id))
                .ok_or_else(|| ScimError::not_found("no such resource type"))?;
            Ok(ScimResponse::ok(resource_type))
        }
        ["Users"] => match request.method {
            Method::Get => list_users(request, ctx),
            Method::Post => create_user(request, ctx),
            _ => Err(ScimError::method(
                "the /Users collection allows GET and POST",
            )),
        },
        ["Users", id] => user_item(request, ctx, id),
        ["Groups"] => match request.method {
            Method::Get => list_groups(request, ctx),
            Method::Post => create_group(request, ctx),
            _ => Err(ScimError::method(
                "the /Groups collection allows GET and POST",
            )),
        },
        ["Groups", id] => group_item(request, ctx, id),
        _ => Err(ScimError::not_found("no such SCIM endpoint")),
    }
}

fn expect(method: Method, allowed: &[Method]) -> ScimResult<()> {
    if allowed.contains(&method) {
        Ok(())
    } else {
        Err(ScimError::method(format!(
            "{} is not allowed here",
            method.as_str()
        )))
    }
}

fn parse_id(text: &str) -> ScimResult<RecordId> {
    parse_record_id(text).ok_or_else(|| ScimError::not_found("no such resource"))
}

/// The directory's enable/disable refusal, mapped to a clean SCIM `400`.
fn directory_refusal(errors: spot_iam_directory::IamErrors) -> ScimError {
    ScimError::invalid_value(errors.to_string())
}

fn page(
    items: Vec<Json>,
    query: &BTreeMap<String, String>,
    limits: &Limits,
) -> ScimResult<(Vec<Json>, usize, usize)> {
    let total = items.len();
    let start_index = match query.get("startIndex") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| ScimError::invalid_value("`startIndex` must be a whole number"))?
            .max(1),
        None => 1,
    };
    let count = match query.get("count") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| ScimError::invalid_value("`count` must be a whole number"))?
            .min(limits.max_count),
        None => limits.default_count.min(limits.max_count),
    };
    let page: Vec<Json> = items
        .into_iter()
        .skip(start_index.saturating_sub(1))
        .take(count)
        .collect();
    Ok((page, total, start_index))
}

fn list_response(resources: Vec<Json>, total: usize, start_index: usize) -> Json {
    json!({
        "schemas": [LIST_SCHEMA],
        "totalResults": total,
        "itemsPerPage": resources.len(),
        "startIndex": start_index,
        "Resources": resources,
    })
}

fn parse_filter(request: &ScimRequest, limits: &Limits) -> ScimResult<Option<Filter>> {
    match request.query.get("filter") {
        None => Ok(None),
        Some(text) => {
            if text.chars().count() > limits.max_filter_chars {
                return Err(ScimError::invalid_filter(format!(
                    "the filter is longer than {} characters",
                    limits.max_filter_chars
                )));
            }
            Ok(Some(Filter::parse(text)?))
        }
    }
}

// ----- Users ---------------------------------------------------------------------------------

fn list_users(request: &ScimRequest, ctx: &mut ScimContext<'_>) -> ScimResult<ScimResponse> {
    let filter = parse_filter(request, ctx.limits)?;
    let mut resources = Vec::new();
    for stored in ctx.store.list_users(&ctx.tenant) {
        let resource = stored.to_resource();
        if let Some(filter) = &filter
            && !filter.matches(&Target::User(&resource))?
        {
            continue;
        }
        resources.push(serde_json::to_value(&resource).unwrap_or(Json::Null));
    }
    let (items, total, start_index) = page(resources, &request.query, ctx.limits)?;
    Ok(ScimResponse::ok(list_response(items, total, start_index)))
}

fn create_user(request: &ScimRequest, ctx: &mut ScimContext<'_>) -> ScimResult<ScimResponse> {
    let body = require_body(request)?;
    let input: ScimUser = serde_json::from_value(body.clone())
        .map_err(|e| ScimError::invalid_syntax(format!("the body is not a user: {e}")))?;
    if input.user_name.trim().is_empty() {
        return Err(ScimError::invalid_value("`userName` is required"));
    }
    if let Some(existing) = ctx
        .store
        .find_user_by_name(&ctx.tenant, input.user_name.trim())
    {
        return Err(ScimError::uniqueness(format!(
            "the userName is already taken by user {}",
            existing.user.id
        )));
    }
    let id = ctx.store.new_user_id();
    let stored = StoredUser::create(ctx.tenant.clone(), id, input, ctx.now)?;
    ctx.store.insert_user(stored.clone())?;
    emit_user_created(ctx, &stored)?;
    ctx.audit(
        "scim_user_create",
        &stored.user.id.to_string(),
        "created through SCIM",
    );
    let resource = stored.to_resource();
    let location = format!("{SCIM_BASE}/Users/{}", stored.user.id);
    Ok(ScimResponse::created(
        serde_json::to_value(&resource).unwrap_or(Json::Null),
        location,
        etag(stored.version),
    ))
}

fn user_item(
    request: &ScimRequest,
    ctx: &mut ScimContext<'_>,
    id: &str,
) -> ScimResult<ScimResponse> {
    let id = parse_id(id)?;
    let existing = ctx
        .store
        .get_user(&ctx.tenant, &id)
        .ok_or_else(|| ScimError::not_found("no such user"))?;
    match request.method {
        Method::Get => {
            let resource = existing.to_resource();
            Ok(
                ScimResponse::ok(serde_json::to_value(&resource).unwrap_or(Json::Null))
                    .with_etag(Some(etag(existing.version))),
            )
        }
        Method::Put => replace_user(request, ctx, existing),
        Method::Patch => patch_user(request, ctx, existing),
        Method::Delete => delete_user(ctx, existing),
        Method::Post => Err(ScimError::method("POST is not allowed on a user")),
    }
}

fn check_if_match(request: &ScimRequest, version: u64) -> ScimResult<()> {
    if let Some(header) = &request.if_match
        && let Some(wanted) = parse_etag(header)
        && wanted != version
    {
        return Err(ScimError::version_conflict(
            "the resource changed since the version you sent",
        ));
    }
    Ok(())
}

fn replace_user(
    request: &ScimRequest,
    ctx: &mut ScimContext<'_>,
    existing: StoredUser,
) -> ScimResult<ScimResponse> {
    check_if_match(request, existing.version)?;
    let body = require_body(request)?;
    let input: ScimUser = serde_json::from_value(body.clone())
        .map_err(|e| ScimError::invalid_syntax(format!("the body is not a user: {e}")))?;
    if let Some(other) = ctx
        .store
        .find_user_by_name(&ctx.tenant, input.user_name.trim())
        && other.user.id != existing.user.id
    {
        return Err(ScimError::uniqueness(format!(
            "the userName is already taken by user {}",
            other.user.id
        )));
    }
    let mut stored = existing.apply_put(&input, ctx.now)?;
    let outcome = settle_user(ctx, &existing, &mut stored)?;
    ctx.store.update_user(stored.clone())?;
    ctx.audit(
        "scim_user_replace",
        &stored.user.id.to_string(),
        "replaced through SCIM",
    );
    let resource = stored.to_resource();
    let mut response = ScimResponse::ok(serde_json::to_value(&resource).unwrap_or(Json::Null))
        .with_etag(Some(etag(stored.version)));
    response.outcome = outcome;
    Ok(response)
}

fn patch_user(
    request: &ScimRequest,
    ctx: &mut ScimContext<'_>,
    existing: StoredUser,
) -> ScimResult<ScimResponse> {
    check_if_match(request, existing.version)?;
    let body = require_body(request)?;
    let patch = PatchRequest::parse(&body)?;
    let patched = patch.apply_to_user(&existing.to_resource())?;
    let mut stored = existing.apply_put(&patched, ctx.now)?;
    let outcome = settle_user(ctx, &existing, &mut stored)?;
    ctx.store.update_user(stored.clone())?;
    ctx.audit(
        "scim_user_patch",
        &stored.user.id.to_string(),
        "patched through SCIM",
    );
    let resource = stored.to_resource();
    let mut response = ScimResponse::ok(serde_json::to_value(&resource).unwrap_or(Json::Null))
        .with_etag(Some(etag(stored.version)));
    response.outcome = outcome;
    Ok(response)
}

fn delete_user(ctx: &mut ScimContext<'_>, existing: StoredUser) -> ScimResult<ScimResponse> {
    let mut outcome = None;
    if existing.user.enabled {
        let mut stored = existing.clone();
        stored.user.enabled = false;
        stored.version += 1;
        stored.updated_at = ctx.now;
        outcome = settle_user(ctx, &existing, &mut stored)?;
        ctx.store.update_user(stored)?;
    }
    ctx.audit(
        "scim_user_deactivate",
        &existing.user.id.to_string(),
        "DELETE deactivates; the record is kept",
    );
    let mut response = ScimResponse::no_content();
    response.outcome = outcome;
    Ok(response)
}

/// Moves a stored user through the directory's enable/disable path when `active` changed, and
/// carries out every leaver action a deactivation asks for. Returns the outcome of a
/// deactivation, or `None` when nothing about enablement changed.
fn settle_user(
    ctx: &mut ScimContext<'_>,
    before: &StoredUser,
    after: &mut StoredUser,
) -> ScimResult<Option<LeaverOutcome>> {
    if before.user.enabled && !after.user.enabled {
        let enabled = User {
            enabled: true,
            deactivated_at: None,
            ..after.user.clone()
        };
        let transition = enabled
            .disable(&ctx.actor(), "deactivated through SCIM", ctx.now)
            .map_err(directory_refusal)?;
        after.user = transition.new_state;
        let delegate = transition.actions.iter().find_map(|a| match a {
            LeaverAction::ReassignOpenTasks { to } => *to,
            _ => None,
        });
        let outcome = apply_leaver_actions(ctx, &transition.actions, &after.user.id, delegate)?;
        emit_user_disabled(ctx, &after.user.id, &transition.actions, &outcome)?;
        ctx.audit(
            "scim_user_deactivate",
            &after.user.id.to_string(),
            "deactivated; leaver actions carried out",
        );
        Ok(Some(outcome))
    } else if !before.user.enabled && after.user.enabled {
        let disabled = User {
            enabled: false,
            deactivated_at: after.user.deactivated_at.or(Some(ctx.now)),
            ..after.user.clone()
        };
        let transition = disabled
            .enable(&ctx.actor(), "reactivated through SCIM", ctx.now)
            .map_err(directory_refusal)?;
        after.user = transition.new_state;
        emit_user_enabled(ctx, &after.user.id)?;
        ctx.audit(
            "scim_user_enable",
            &after.user.id.to_string(),
            "reactivated through SCIM",
        );
        Ok(None)
    } else {
        Ok(None)
    }
}

/// Carries out every leaver action, in the fixed order the directory returned them.
pub fn apply_leaver_actions(
    ctx: &mut ScimContext<'_>,
    actions: &[LeaverAction],
    user: &RecordId,
    delegate: Option<RecordId>,
) -> ScimResult<LeaverOutcome> {
    let mut outcome = LeaverOutcome {
        user: Some(*user),
        ..LeaverOutcome::default()
    };
    let tenant = ctx.tenant.clone();
    for action in actions {
        match action {
            LeaverAction::EndSessions => {
                outcome.sessions_ended = ctx.store.end_sessions(&tenant, user);
            }
            LeaverAction::RevokeRefreshTokens => {
                outcome.refresh_tokens_revoked = ctx.store.revoke_refresh_tokens(&tenant, user);
            }
            LeaverAction::DisableApiKeys => {
                outcome.api_keys_disabled = ctx.store.disable_api_keys(&tenant, user);
            }
            LeaverAction::CancelInvitations => {
                outcome.invitations_cancelled = ctx.store.cancel_invitations(&tenant, user);
            }
            LeaverAction::ReassignOpenTasks { to } => {
                outcome.tasks_reassigned =
                    ctx.store
                        .reassign_open_tasks(&tenant, user, to.or(delegate));
            }
            LeaverAction::WriteAudit => outcome.audit_written = true,
        }
    }
    Ok(outcome)
}

fn leaver_action_name(action: &LeaverAction) -> &'static str {
    match action {
        LeaverAction::EndSessions => "end_sessions",
        LeaverAction::RevokeRefreshTokens => "revoke_refresh_tokens",
        LeaverAction::DisableApiKeys => "disable_api_keys",
        LeaverAction::CancelInvitations => "cancel_invitations",
        LeaverAction::ReassignOpenTasks { .. } => "reassign_open_tasks",
        LeaverAction::WriteAudit => "write_audit",
    }
}

fn emit_user_created(ctx: &mut ScimContext<'_>, stored: &StoredUser) -> ScimResult<()> {
    ctx.store.emit(ScimEvent {
        tenant: ctx.tenant.clone(),
        topic: "identity_user_created".to_owned(),
        data: json!({
            "user": stored.user.id.to_string(),
            "user_type": "internal",
            "source": "scim",
        }),
    })
}

fn emit_user_disabled(
    ctx: &mut ScimContext<'_>,
    user: &RecordId,
    actions: &[LeaverAction],
    outcome: &LeaverOutcome,
) -> ScimResult<()> {
    let actions: Vec<String> = actions
        .iter()
        .map(|a| leaver_action_name(a).to_owned())
        .collect();
    let _ = outcome;
    ctx.store.emit(ScimEvent {
        tenant: ctx.tenant.clone(),
        topic: "identity_user_disabled".to_owned(),
        data: json!({
            "user": user.to_string(),
            "source": "scim",
            "reason": "deactivated through SCIM",
            "actions": actions,
        }),
    })
}

fn emit_user_enabled(ctx: &mut ScimContext<'_>, user: &RecordId) -> ScimResult<()> {
    ctx.store.emit(ScimEvent {
        tenant: ctx.tenant.clone(),
        topic: "identity_user_enabled".to_owned(),
        data: json!({
            "user": user.to_string(),
            "reason": "reactivated through SCIM",
        }),
    })
}

mod groups;
mod static_endpoints;

use groups::*;
use static_endpoints::*;
