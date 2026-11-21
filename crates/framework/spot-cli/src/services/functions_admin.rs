//! The admin function listing: `platform/functions/list` (WP-PLT-17.T7).
//!
//! This file holds the whole behaviour as a plain helper module (it declares no `ORDER` or
//! `register`, so the services registry does not start it): the answer's shape, the
//! Administrator check and a [`FunctionHandler`] that joins them. Mounting it needs two things
//! this task could not touch (`services/function.rs` and `apps/platform/app.toml`); the report
//! names them. Once mounted, the handler is the function `platform/functions/list` (`GET`, not
//! guest) and answers only the Administrator: any other caller, a guest and a platform component
//! alike, gets exactly the answer of an unknown function (`404 method.not_found`).
//!
//! The answer lists `{key, app, module, name, verbs, guest, limit, arguments_schema}` per
//! function and never a handler, a rate-limit bucket or a caller.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value as Json, json};
use spot_app_api::AppError;
use spot_app_api::error::ErrorClass;
use spot_document::engine::PrincipalSource;
use spot_functions::{Caller, Context, FunctionHandler, FunctionInfo, HandlerFuture, verb_name};
use spot_storage_contract::RecordId;

/// The function's address.
pub const APP: &str = "platform";
/// The function's module.
pub const MODULE: &str = "functions";
/// The function's name.
pub const NAME: &str = "list";

/// Whether the caller named by this user id (text) may read the listing.
pub type AdminCheck =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// The current function list of the site.
pub type Lister = Arc<dyn Fn() -> Vec<FunctionInfo> + Send + Sync>;

/// The answer for `infos`: one object per function, in the registry's order.
#[must_use]
pub fn listing(infos: &[FunctionInfo]) -> Json {
    let rows: Vec<Json> = infos
        .iter()
        .map(|info| {
            json!({
                "key": info.key.to_string(),
                "app": info.app,
                "module": info.key.module,
                "name": info.key.name,
                "verbs": info.verbs.as_ref().map(|verbs| {
                    verbs.iter().map(|v| verb_name(*v)).collect::<Vec<_>>()
                }),
                "guest": info.guest,
                "limit": info.limit.as_ref().map(|l| json!({
                    "permits": l.permits,
                    "per_seconds": l.per.as_secs(),
                })),
                "arguments_schema": info.arguments,
            })
        })
        .collect();
    json!({ "functions": rows })
}

/// An Administrator check over the site's principals: a user holds `Tenant Administrator` by an
/// assignment with no scope restriction ("Tenant Administrator for Company B" is not one). A text
/// that is not a record id, and an unknown user, are not administrators.
#[must_use]
pub fn administrator_check(principals: Arc<dyn PrincipalSource>) -> AdminCheck {
    Arc::new(move |user: String| {
        let principals = Arc::clone(&principals);
        Box::pin(async move {
            let Ok(parsed) = uuid::Uuid::parse_str(&user) else {
                return false;
            };
            let Ok(id) = RecordId::from_bytes(*parsed.as_bytes()) else {
                return false;
            };
            let principal = principals.resolve(&id).await;
            principal.assignments.iter().any(|assignment| {
                assignment.scope.is_empty()
                    && assignment
                        .roles
                        .iter()
                        .any(|role| role.0 == spot_platform_app::role::TENANT_ADMINISTRATOR)
            })
        })
    })
}

/// The handler of `platform/functions/list`.
pub struct ListHandler {
    lister: Lister,
    admin: AdminCheck,
}

impl ListHandler {
    /// A handler over the site's function list and Administrator check.
    #[must_use]
    pub fn new(lister: Lister, admin: AdminCheck) -> Self {
        Self { lister, admin }
    }
}

/// The answer of an unknown function, so a caller who may not read the listing learns nothing.
fn not_found() -> AppError {
    AppError::new(
        ErrorClass::MethodNotFound,
        "function.not_found",
        "No such function.",
    )
}

impl FunctionHandler for ListHandler {
    fn call<'a>(&'a self, ctx: &'a Context, _args: Json) -> HandlerFuture<'a> {
        Box::pin(async move {
            let Caller::User(user) = &ctx.caller else {
                return Err(not_found());
            };
            if !(self.admin)(user.clone()).await {
                return Err(not_found());
            }
            Ok(listing(&(self.lister)()))
        })
    }
}
