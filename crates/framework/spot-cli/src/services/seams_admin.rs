//! The admin override listing: `platform/seams/overrides` (WP-EXT-03.T2).
//!
//! A plain helper module (no `ORDER` or `register`, so the services registry does not start it):
//! the answer's shape, the Administrator check and a [`FunctionHandler`] joining them. Mounting
//! it as a function needs `services/function.rs` and `apps/platform/app.toml`, outside this
//! task's paths (the same stop as WP-PLT-17.T7); the report names the two lines. `spot overrides
//! list` uses [`listing`] too, so the command and the method print the same data.
//!
//! The answer is `{ overrides, seams, conflicts }`: names, kinds and reasons only, never step
//! code, arguments or a handler. Any caller but the Administrator gets the answer of an unknown
//! function.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value as Json, json};
use spot_app_api::AppError;
use spot_app_api::error::ErrorClass;
use spot_document::engine::PrincipalSource;
use spot_functions::{Caller, Context, FunctionHandler, HandlerFuture};
use spot_seams::OverrideReport;
use spot_storage_contract::RecordId;

/// The function's address.
pub const APP: &str = "platform";
/// The function's module.
pub const MODULE: &str = "seams";
/// The function's name.
pub const NAME: &str = "overrides";

/// Whether the caller named by this user id (text) may read the listing.
pub type AdminCheck =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// The site's current override report (from the registry the host holds, so a reload shows).
pub type Reporter = Arc<dyn Fn() -> OverrideReport + Send + Sync>;

/// The answer: the report's rows (the library's stable order and field names) and the conflicts
/// (empty for a registry that built; a conflict stops the build, so a served site has none).
#[must_use]
pub fn listing(report: &OverrideReport, conflicts: &[String]) -> Json {
    let mut value = serde_json::to_value(report).unwrap_or_else(|_| json!({}));
    if let Some(map) = value.as_object_mut() {
        map.insert("conflicts".to_owned(), json!(conflicts));
    }
    value
}

fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0_u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// An Administrator check over the site's principals: `Tenant Administrator` by an assignment
/// with no scope restriction. Not a record id, or an unknown user, is not an administrator.
#[must_use]
pub fn administrator_check(principals: Arc<dyn PrincipalSource>) -> AdminCheck {
    Arc::new(move |user: String| {
        let principals = Arc::clone(&principals);
        Box::pin(async move {
            let Some(bytes) = parse_uuid(&user) else {
                return false;
            };
            let Ok(id) = RecordId::from_bytes(bytes) else {
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

/// The handler of `platform/seams/overrides`.
pub struct OverridesHandler {
    reporter: Reporter,
    admin: AdminCheck,
}

impl OverridesHandler {
    /// A handler over the site's report and Administrator check.
    #[must_use]
    pub fn new(reporter: Reporter, admin: AdminCheck) -> Self {
        Self { reporter, admin }
    }
}

fn not_found() -> AppError {
    AppError::new(
        ErrorClass::MethodNotFound,
        "function.not_found",
        "No such function.",
    )
}

impl FunctionHandler for OverridesHandler {
    fn call<'a>(&'a self, ctx: &'a Context, _args: Json) -> HandlerFuture<'a> {
        Box::pin(async move {
            let Caller::User(user) = &ctx.caller else {
                return Err(not_found());
            };
            if !(self.admin)(user.clone()).await {
                return Err(not_found());
            }
            Ok(listing(&(self.reporter)(), &[]))
        })
    }
}
