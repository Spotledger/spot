//! SCIM `/Groups` handlers.

use spot_iam_roles::OutsideGuard;

use super::*;

// `request.rs` still imports the unguarded `plan_idp` (it is outside this task's owned paths);
// naming it here keeps that import used until a follow-up removes it.
#[allow(unused_imports)]
use super::plan_idp as _;

// ----- Groups --------------------------------------------------------------------------------

pub(super) fn list_groups(
    request: &ScimRequest,
    ctx: &mut ScimContext<'_>,
) -> ScimResult<ScimResponse> {
    let filter = parse_filter(request, ctx.limits)?;
    let users = ctx.store.list_users(&ctx.tenant);
    let display_of = |id: &RecordId| -> Option<String> {
        users
            .iter()
            .find(|u| &u.user.id == id)
            .map(|u| u.user.display_name.clone())
    };
    let mut resources = Vec::new();
    for stored in ctx.store.list_groups(&ctx.tenant) {
        let resource = stored.to_resource(&display_of);
        if let Some(filter) = &filter
            && !filter.matches(&Target::Group(&resource))?
        {
            continue;
        }
        resources.push(serde_json::to_value(&resource).unwrap_or(Json::Null));
    }
    let (items, total, start_index) = page(resources, &request.query, ctx.limits)?;
    Ok(ScimResponse::ok(list_response(items, total, start_index)))
}

pub(super) fn resolve_members(
    ctx: &ScimContext<'_>,
    members: &[Member],
) -> ScimResult<Vec<RecordId>> {
    let mut ids = Vec::new();
    for member in members {
        let id = parse_record_id(&member.value)
            .ok_or_else(|| ScimError::invalid_value("a member value is not a user id"))?;
        if ctx.store.get_user(&ctx.tenant, &id).is_none() {
            return Err(ScimError::invalid_value(
                "a member is not a user of this tenant",
            ));
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

pub(super) fn create_group(
    request: &ScimRequest,
    ctx: &mut ScimContext<'_>,
) -> ScimResult<ScimResponse> {
    let body = require_body(request)?;
    let input: ScimGroup = serde_json::from_value(body.clone())
        .map_err(|e| ScimError::invalid_syntax(format!("the body is not a group: {e}")))?;
    if input.display_name.trim().is_empty() {
        return Err(ScimError::invalid_value("`displayName` is required"));
    }
    if let Some(existing) = ctx
        .store
        .find_group_by_name(&ctx.tenant, input.display_name.trim())
    {
        return Err(ScimError::uniqueness(format!(
            "the displayName is already taken by group {}",
            existing.id
        )));
    }
    let members = resolve_members(ctx, &input.members)?;
    let id = ctx.store.new_group_id();
    let stored = StoredGroup {
        tenant: ctx.tenant.clone(),
        id,
        external_id: input.external_id,
        display_name: input.display_name.trim().to_owned(),
        members,
        version: 1,
        created_at: ctx.now,
        updated_at: ctx.now,
    };
    ctx.store.insert_group(stored.clone())?;
    reconcile_idp(ctx)?;
    ctx.audit("scim_group_create", &id.to_string(), "created through SCIM");
    let users = ctx.store.list_users(&ctx.tenant);
    let resource = stored.to_resource(&|id| {
        users
            .iter()
            .find(|u| &u.user.id == id)
            .map(|u| u.user.display_name.clone())
    });
    let location = format!("{SCIM_BASE}/Groups/{id}");
    Ok(ScimResponse::created(
        serde_json::to_value(&resource).unwrap_or(Json::Null),
        location,
        etag(stored.version),
    ))
}

pub(super) fn group_item(
    request: &ScimRequest,
    ctx: &mut ScimContext<'_>,
    id: &str,
) -> ScimResult<ScimResponse> {
    let id = parse_id(id)?;
    let existing = ctx
        .store
        .get_group(&ctx.tenant, &id)
        .ok_or_else(|| ScimError::not_found("no such group"))?;
    match request.method {
        Method::Get => {
            let users = ctx.store.list_users(&ctx.tenant);
            let resource = existing.to_resource(&|id| {
                users
                    .iter()
                    .find(|u| &u.user.id == id)
                    .map(|u| u.user.display_name.clone())
            });
            Ok(
                ScimResponse::ok(serde_json::to_value(&resource).unwrap_or(Json::Null))
                    .with_etag(Some(etag(existing.version))),
            )
        }
        Method::Put => replace_group(request, ctx, existing),
        Method::Patch => patch_group(request, ctx, existing),
        Method::Delete => {
            ctx.store.delete_group(&ctx.tenant, &existing.id);
            reconcile_idp(ctx)?;
            ctx.audit(
                "scim_group_delete",
                &existing.id.to_string(),
                "deleted through SCIM; users are never deleted",
            );
            Ok(ScimResponse::no_content())
        }
        Method::Post => Err(ScimError::method("POST is not allowed on a group")),
    }
}

pub(super) fn replace_group(
    request: &ScimRequest,
    ctx: &mut ScimContext<'_>,
    existing: StoredGroup,
) -> ScimResult<ScimResponse> {
    check_if_match(request, existing.version)?;
    let body = require_body(request)?;
    let input: ScimGroup = serde_json::from_value(body.clone())
        .map_err(|e| ScimError::invalid_syntax(format!("the body is not a group: {e}")))?;
    if input.display_name.trim().is_empty() {
        return Err(ScimError::invalid_value("`displayName` is required"));
    }
    if let Some(other) = ctx
        .store
        .find_group_by_name(&ctx.tenant, input.display_name.trim())
        && other.id != existing.id
    {
        return Err(ScimError::uniqueness(format!(
            "the displayName is already taken by group {}",
            other.id
        )));
    }
    let members = resolve_members(ctx, &input.members)?;
    let stored = StoredGroup {
        tenant: ctx.tenant.clone(),
        id: existing.id,
        external_id: input.external_id,
        display_name: input.display_name.trim().to_owned(),
        members,
        version: existing.version + 1,
        created_at: existing.created_at,
        updated_at: ctx.now,
    };
    ctx.store.update_group(stored.clone())?;
    reconcile_idp(ctx)?;
    ctx.audit(
        "scim_group_replace",
        &stored.id.to_string(),
        "replaced through SCIM",
    );
    Ok(
        ScimResponse::ok(serde_json::to_value(group_json(ctx, &stored)).unwrap_or(Json::Null))
            .with_etag(Some(etag(stored.version))),
    )
}

pub(super) fn patch_group(
    request: &ScimRequest,
    ctx: &mut ScimContext<'_>,
    existing: StoredGroup,
) -> ScimResult<ScimResponse> {
    check_if_match(request, existing.version)?;
    let body = require_body(request)?;
    let patch = PatchRequest::parse(&body)?;
    let users = ctx.store.list_users(&ctx.tenant);
    let display_of = |id: &RecordId| -> Option<String> {
        users
            .iter()
            .find(|u| &u.user.id == id)
            .map(|u| u.user.display_name.clone())
    };
    let patched = patch.apply_to_group(&existing.to_resource(&display_of))?;
    if patched.display_name.trim().is_empty() {
        return Err(ScimError::invalid_value("`displayName` is required"));
    }
    let members = resolve_members(ctx, &patched.members)?;
    let stored = StoredGroup {
        tenant: ctx.tenant.clone(),
        id: existing.id,
        external_id: patched.external_id,
        display_name: patched.display_name.trim().to_owned(),
        members,
        version: existing.version + 1,
        created_at: existing.created_at,
        updated_at: ctx.now,
    };
    ctx.store.update_group(stored.clone())?;
    reconcile_idp(ctx)?;
    ctx.audit(
        "scim_group_patch",
        &stored.id.to_string(),
        "patched through SCIM",
    );
    Ok(
        ScimResponse::ok(serde_json::to_value(group_json(ctx, &stored)).unwrap_or(Json::Null))
            .with_etag(Some(etag(stored.version))),
    )
}

pub(super) fn group_json(ctx: &ScimContext<'_>, group: &StoredGroup) -> Json {
    let users = ctx.store.list_users(&ctx.tenant);
    let resource = group.to_resource(&|id| {
        users
            .iter()
            .find(|u| &u.user.id == id)
            .map(|u| u.user.display_name.clone())
    });
    serde_json::to_value(&resource).unwrap_or(Json::Null)
}

/// Recomputes the idp-sourced assignments from the current groups and the mapping table. It
/// reads the role records to validate the mapping and **never writes one**.
pub(super) fn reconcile_idp(ctx: &mut ScimContext<'_>) -> ScimResult<()> {
    let mappings: Vec<GroupMapping> = ctx.store.mappings(&ctx.tenant);
    let roles = ctx.store.role_directory();
    let map = build_group_role_map(&mappings, &roles)?;
    let users = ctx.store.list_users(&ctx.tenant);
    let groups = ctx.store.list_groups(&ctx.tenant);
    let user_groups: Vec<(String, Vec<String>)> = users
        .iter()
        .map(|u| {
            let uid = u.user.id.to_string();
            let names: Vec<String> = groups
                .iter()
                .filter(|g| g.members.contains(&u.user.id))
                .map(|g| g.display_name.clone())
                .collect();
            (uid, names)
        })
        .collect();
    let current = ctx.store.assignments(&ctx.tenant);
    // ADR-083 Barrier 3: a group mapping must never give a portal user a staff role. The store
    // exposes role records but not duties, so the guard is built blind: a portal user is given
    // only a portal role that has no duties and no direct rights (fail closed).
    let portal: Vec<String> = users
        .iter()
        .filter(|u| u.user.user_type == UserType::Portal)
        .map(|u| u.user.id.to_string())
        .collect();
    let guard = OutsideGuard::from_roles(&ctx.store.role_records(), portal);
    let mut ids = StoreIds(&mut *ctx.store);
    let plan = spot_iam_assign::plan_idp_guarded(
        &map,
        &mut ids,
        &user_groups,
        true,
        &current,
        ctx.now,
        &guard,
    );
    ctx.store.apply_assignments(&ctx.tenant, &plan, ctx.now)?;
    Ok(())
}

/// The store's record-id source, as the reconciler's id source: a granted assignment carries
/// the id the store would persist.
struct StoreIds<'a>(&'a mut dyn ScimStore);

impl spot_iam_assign::IdSource for StoreIds<'_> {
    fn next(&mut self) -> String {
        self.0.new_assignment_id().to_string()
    }
}
