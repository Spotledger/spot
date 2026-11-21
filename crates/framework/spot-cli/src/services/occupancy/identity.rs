//! Who is asking: the principal of one occupancy request.
//!
//! The holder of a claim is **always** the authenticated principal (`user:<id>` for staff,
//! `consumer:<id>` for a consumer principal, ADR-083). Nothing in a request body, query string or
//! header selects it. A caller with no valid credential gets the service's own answer
//! (`auth.required` or `auth.invalid_credentials`), never a partial one.

use std::sync::Arc;

use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_occupancy::Holder;

use crate::audit::PathClass;
use crate::live::SiteLive;

/// The platform permission that lets a staff user read the occupancy rows and confirm or release
/// any hold. Held by nobody by default: the app's grants name the roles.
pub const MANAGE: &str = "occupancy_manage";

/// The authenticated caller of one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Who {
    /// The typed holder reference the caller claims as.
    pub holder: Holder,
    /// Whether the caller holds [`MANAGE`]. Never true for a consumer.
    pub manage: bool,
}

/// Resolves the caller of a request.
pub trait Identity: Send + Sync {
    /// The caller of `ctx`. An unauthenticated or restricted credential is an `ApiError`
    /// (`auth.*` or `permission.denied`); a service that cannot answer is `server.unavailable`.
    fn who<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<Who, ApiError>>;
}

/// Resolves a consumer principal (ADR-083) from the request's credential.
///
/// No module provides one yet: the consumer sign-in service is a later task. When a module
/// provides this port (`ctx.provide::<dyn ConsumerIdentity>(..)` at a smaller `ORDER`), a bearer
/// that is not a staff credential is offered to it. The answer is the consumer's id, which must
/// follow the holder grammar (1 to 64 of `A-Z a-z 0-9 - _`); anything else is refused.
pub trait ConsumerIdentity: Send + Sync {
    /// The consumer's id for the credential in `ctx`, or `None` when it names no live consumer
    /// session on `ctx.site`.
    fn consumer<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Option<String>>;
}

/// The production [`Identity`]: the site's sign-in service, its permission snapshot and, when a
/// module provides one, the consumer resolver.
pub struct LiveIdentity {
    live: Arc<SiteLive>,
    consumers: Option<Arc<dyn ConsumerIdentity>>,
}

impl LiveIdentity {
    /// Identity over `live`, with an optional consumer resolver.
    pub fn new(live: Arc<SiteLive>, consumers: Option<Arc<dyn ConsumerIdentity>>) -> Self {
        Self { live, consumers }
    }
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable)
}

impl Identity for LiveIdentity {
    fn who<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<Who, ApiError>> {
        Box::pin(async move {
            let auth = self.live.auth().ok_or_else(unavailable)?;
            match auth.authenticate_on(ctx, PathClass::Api).await {
                Ok(who) => {
                    // A session that may only choose a password acts as nobody.
                    if auth.is_restricted(&who) {
                        return Err(ApiError::from_code(Code::PermissionDenied));
                    }
                    let principals = self.live.principals().ok_or_else(unavailable)?;
                    let principal = DocPrincipals::resolve(&*principals, &who.user_id).await;
                    let manage = self
                        .live
                        .served()
                        .is_some_and(|s| s.decision.app_permission(&principal, MANAGE).allowed);
                    let holder =
                        Holder::user(&who.user_id.to_string()).map_err(|_| unavailable())?;
                    Ok(Who { holder, manage })
                }
                Err(refused) => {
                    // Not a staff credential: it may still be a consumer's.
                    if let Some(resolver) = &self.consumers
                        && let Some(id) = resolver.consumer(ctx).await
                        && let Ok(holder) = Holder::consumer(&id)
                    {
                        return Ok(Who {
                            holder,
                            manage: false,
                        });
                    }
                    Err(refused)
                }
            }
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_manage_permission_has_a_stable_name() {
        assert_eq!(MANAGE, "occupancy_manage");
    }

    #[test]
    fn consumer_ids_follow_the_holder_grammar() {
        assert!(Holder::consumer("c-1_A").is_ok());
        assert!(Holder::consumer("c:1").is_err());
        assert!(Holder::consumer("").is_err());
        assert!(Holder::consumer(&"x".repeat(65)).is_err());
    }
}
