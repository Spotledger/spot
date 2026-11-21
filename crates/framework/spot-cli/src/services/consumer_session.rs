//! Consumer sessions reach the occupancy port (WP-STO-14.T4; ADR-083).
//!
//! A consumer is a separate principal at the edge of the same tenant. The occupancy module asks
//! one question of a bearer that is not a staff credential: *which consumer, if any, holds this
//! session on this site?* ([`ConsumerIdentity`]). This module answers it from a
//! [`ConsumerSessions`] port, the consumer login and session service (ADR-083 task 14), which is
//! **not built yet**:
//!
//! - With no `ConsumerSessions` port the module is inert: it provides nothing, the occupancy
//!   module sees no consumer resolver, and a consumer bearer is refused as any unknown credential
//!   is (`auth.invalid_credentials`).
//! - When task 14's service provides the port (at an `ORDER` below this module's), the adapter
//!   [`SessionConsumers`] turns a live consumer session into the consumer's id and provides
//!   `dyn ConsumerIdentity` for the occupancy module (`ORDER` 1170, after this one).
//!
//! # What a consumer may do
//!
//! The occupancy adapter already gives a consumer the rights ADR-083 allows: it claims for itself
//! (the holder is `consumer:<id>`, never a request field), confirms and releases **its own** holds,
//! never reads another's, and never holds `occupancy_manage`. Every other route refuses a consumer
//! at the audience gate of the router (WP-IAM-11.T2). [`CONSUMER_OPERATIONS`] lists the occupancy
//! operations a consumer session is offered, for the operator guide and the tests.
//!
//! # Fail closed
//!
//! The adapter answers `None` (so the credential is refused) when the session belongs to another
//! site, when the id does not follow the holder grammar, or when the request has no credential.
//! It never logs a token or an id.

use std::sync::Arc;

use spot_core::SiteId;
use spot_http::api::service::{BoxFuture, Credential, RequestCtx};
use spot_occupancy::Holder;

use crate::Result;
use crate::services::SiteCtx;
use crate::services::occupancy::ConsumerIdentity;

/// Start order: after the sign-in services, before the occupancy module (1170) that takes the
/// resolver this module provides.
pub const ORDER: u32 = 1160;

/// The occupancy operations a consumer session is offered. Anything else answers the consumer
/// with the catalogued refusal.
pub const CONSUMER_OPERATIONS: [&str; 3] = ["claim", "confirm", "release"];

/// One live consumer session, as the consumer login service reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumerSession {
    /// The site the session was issued on.
    pub site: SiteId,
    /// The consumer's id (the holder grammar: 1 to 64 of `A-Z a-z 0-9 - _`).
    pub consumer_id: String,
}

/// The consumer login and session service, seen from here: it knows live sessions, the occupancy
/// module does not. A session that is expired, revoked or unknown answers `None`.
pub trait ConsumerSessions: Send + Sync {
    /// The live consumer session for `credential`, if any.
    fn session<'a>(&'a self, credential: &'a Credential) -> BoxFuture<'a, Option<ConsumerSession>>;
}

/// [`ConsumerIdentity`] over a [`ConsumerSessions`] port.
pub struct SessionConsumers {
    sessions: Arc<dyn ConsumerSessions>,
}

impl SessionConsumers {
    /// A resolver over `sessions`.
    #[must_use]
    pub fn new(sessions: Arc<dyn ConsumerSessions>) -> Self {
        Self { sessions }
    }
}

impl ConsumerIdentity for SessionConsumers {
    fn consumer<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let credential = ctx.credential.as_ref()?;
            let session = self.sessions.session(credential).await?;
            // Tenant edge: a session of another site is no session here.
            if session.site != ctx.site {
                return None;
            }
            // The holder grammar is the occupancy service's; refuse what it would refuse.
            Holder::consumer(&session.consumer_id).ok()?;
            Some(session.consumer_id)
        })
    }
}

/// Provides `dyn ConsumerIdentity` when a consumer session service exists.
///
/// # Errors
///
/// Another module already provides the resolver.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let Some(sessions) = ctx.port::<dyn ConsumerSessions>() else {
        // No consumer service on this build: nothing to resolve, nothing provided.
        return Ok(());
    };
    let resolver: Arc<dyn ConsumerIdentity> = Arc::new(SessionConsumers::new(sessions));
    ctx.provide::<dyn ConsumerIdentity>(resolver)
}
