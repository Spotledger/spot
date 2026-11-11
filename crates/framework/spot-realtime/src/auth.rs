//! The permission seam: [`RoomAuthorizer`].

use std::future::Future;
use std::future::ready;
use std::pin::Pin;

use spot_core::SiteId;

use crate::error::Denied;
use crate::event::Principal;
use crate::room::Room;

/// The boxed future [`RoomAuthorizer::authorize`] returns. The trait must stay object-safe
/// (`Arc<dyn RoomAuthorizer>`), so async work is erased behind a pin-boxed future.
pub type AuthFuture<'a> = Pin<Box<dyn Future<Output = Result<(), Denied>> + Send + 'a>>;

/// The seam between the bus and the permission rules.
///
/// The bus calls [`authorize`](RoomAuthorizer::authorize) on **every** join, including joins
/// made while resuming a subscription after reconnect — permissions can change between
/// connections. The rules themselves are WP-PLT-03; this crate only asks and obeys.
///
/// The bus awaits this future **outside** its internal state lock (no `std::sync` lock is
/// ever held across an `.await`), so an authorizer may itself be async and do I/O.
pub trait RoomAuthorizer: Send + Sync {
    /// Decides whether `principal` may receive events from `room` on `site`. `Ok(())`
    /// admits the join; `Err(Denied)` refuses it and the bus never enrols the subscriber.
    fn authorize(&self, site: &SiteId, principal: &Principal, room: &Room) -> AuthFuture<'_>;
}

/// An authorizer that admits every join — for tests and for deployments where permission
/// checks happen at a different layer before `subscribe` is ever called.
#[derive(Clone, Debug, Default)]
pub struct AllowAll;

impl RoomAuthorizer for AllowAll {
    fn authorize(&self, _site: &SiteId, _principal: &Principal, _room: &Room) -> AuthFuture<'_> {
        Box::pin(ready(Ok(())))
    }
}
