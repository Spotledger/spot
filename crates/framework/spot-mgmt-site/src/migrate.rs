//! The ten `site.migrate.*` operations as management handlers. Their catalogue entries and their
//! verbs live in `spot-migration-ops` (shared with `spot migrate <verb>`): this module only
//! builds the lifecycle context of the operator who issued the command (channel `mgmt`), looks
//! the verb up and maps its answer. When the host has not wired the lifecycle, every verb
//! refuses with the management code of `migration.port_unavailable` and nothing runs.

use serde_json::{Value, json};
use spot_mgmt_api::handlers::{HandlerError, HandlerRequest, HandlerResult};
use spot_mgmt_ops::{Shell, render};
use spot_migration_ops::{Actor, Channel, LifecycleError};
use spot_permissions::Principal;

use crate::handlers::site_of;
use crate::ports::SiteContext;

/// Runs the migration verb the request names.
pub(crate) async fn run(ctx: &SiteContext, request: &HandlerRequest) -> HandlerResult {
    site_of(ctx, request)?;
    let Some(port) = ctx.migrations.as_ref() else {
        return Err(HandlerError::failed(
            LifecycleError::PortUnavailable { port: "lifecycle" }.mgmt_code(),
        ));
    };
    let principal = Principal::user(&request.principal, Vec::new());
    let actor = Actor::new(principal, Channel::Mgmt)
        .map_err(|error| HandlerError::failed(error.mgmt_code()))?;
    let lifecycle = port
        .context(actor)
        .map_err(|error| HandlerError::failed(error.mgmt_code()))?;
    let registry = spot_migration_ops::handlers(&lifecycle);
    let Some(verb) = registry.get(&request.operation) else {
        return Err(HandlerError::failed("mgmt.unknown_operation"));
    };
    let handled = verb
        .handle(&request.params)
        .await
        .map_err(|error| HandlerError::failed(error.mgmt_code()))?;
    let fields: serde_json::Map<String, Value> = handled
        .fields
        .into_iter()
        .map(|(name, value)| (name, Value::String(value)))
        .collect();
    Ok(json!({
        "message": handled.message,
        "fields": Value::Object(fields),
        "cli": render(&request.params, Shell::Posix),
    }))
}
