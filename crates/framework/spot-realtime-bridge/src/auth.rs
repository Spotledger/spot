//! The permission-checked rooms: [`PermissionRoomAuthorizer`], the
//! [`RoomAuthorizer`](spot_realtime::RoomAuthorizer) over the decision core
//! (`spot-permissions`), with [`PermissionRoomAuthorizer::revalidate`] for snapshot swaps.
//!
//! The rules, exactly as the contract's room table words them:
//!
//! | Room | Who may join |
//! | --- | --- |
//! | `site` | any signed-in non-guest |
//! | `user:<id>` | only that user |
//! | `doctype:<dt>` | anyone who may read the DocType's list |
//! | `doc:<dt>:<id>` | anyone the engine admits with `can(Read)` on the document |
//!
//! Guests are refused everything; the contract declares no public rooms yet. Every denial
//! carries the identical message (`DENY_MESSAGE`) whether the document is missing, the
//! DocType is unknown, the principal has no assignments, or the read is genuinely denied:
//! a denial must not reveal whether the resource exists.
//!
//! # Where the decisions come from
//!
//! The wire principal (`spot_realtime::Principal`) is an opaque id and a guest flag; the
//! decision core evaluates a world of assignments. Three seams inject the mapping:
//! [`PrincipalSource`]
//! (wire id → [`spot_permissions::model::Principal`]), [`EngineSource`] (the live
//! [`PermissionEngine`] over the published snapshot) and [`DocFactsSource`]
//! (doctype+id → the committed [`DocFacts`] a document decision asks). The host wires the
//! engine-backed implementations; every seam can be faked in tests.
//!
//! `doctype:<dt>` asks `can(Read)` on a fact-less document: the verdict admits whoever
//! holds the read right through an assignment that covers the DocType — the "may read the
//! list" question. A list carries no values, so a value restriction cannot be involved.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_core::SiteId;
use spot_permissions::{Action, DocFacts, PermissionEngine};
use spot_realtime::{AuthFuture, Denied, Principal, Room};

use crate::error::{CODE_BAD_PRINCIPAL, CODE_DOCTYPE_UNKNOWN};

/// The one denial message every refused join carries.
pub const DENY_MESSAGE: &str = "you may not join this room";

/// A lookup that failed before a permission question could be asked. Its code is for logs
/// only — the client-visible [`Denied`] is re-worded to [`DENY_MESSAGE`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum LookupFailure {
    /// The DocType (or document) is not in the permission snapshot's world, or the facts
    /// could not be loaded: refused without revealing which of the two it was.
    #[error("{CODE_DOCTYPE_UNKNOWN}: the requested room cannot be verified")]
    UnknownDoctype,
    /// The principal's permission world could not be loaded: refused.
    #[error("{CODE_BAD_PRINCIPAL}: the signed-in principal cannot be resolved")]
    UnresolvedPrincipal,
}

impl LookupFailure {
    /// The stable code of the refusal's cause, for logs and the audit trail.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::UnknownDoctype => CODE_DOCTYPE_UNKNOWN,
            Self::UnresolvedPrincipal => CODE_BAD_PRINCIPAL,
        }
    }
}

/// Answers with the live [`PermissionEngine`] of the currently published permission
/// snapshot. The host swaps implementations (or the `Arc` behind them) when a snapshot is
/// published. The engine is a cheap `Clone` handle over one immutable snapshot, so each
/// decision is internally consistent however the swaps interleave with joins.
pub trait EngineSource: Send + Sync {
    /// The engine over the currently published snapshot.
    fn current(&self) -> PermissionEngine;
}

/// Loads the committed document facts a `doc:` room decision asks for.
///
/// The engine-backed implementation (wired in the host binary) loads the document's
/// `created_by`, lifecycle state and link/select values from storage and maps them onto
/// [`DocFacts`]. A `None` answer (the document does not exist, or the facts cannot be
/// loaded) makes the join denied with [`DENY_MESSAGE`] — the same message as an engine
/// denial — so a refused join never reveals whether the document exists.
pub trait DocFactsSource: Send + Sync {
    /// The facts of `(doctype, id)`, or `None` when they cannot be loaded. Boxed so the
    /// trait stays object-safe, as `RoomAuthorizer` itself is.
    fn facts<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<DocFacts>> + Send + 'a>>;
}

/// Maps a wire principal onto the permission world the decisions evaluate.
///
/// A `None` answer (unknown user, disabled account, lookup failed) denies the join with
/// [`DENY_MESSAGE`].
pub trait PrincipalSource: Send + Sync {
    /// The permission principal of the wire principal `id`, or `None` when unknown.
    /// Boxed so the trait stays object-safe.
    fn principal<'a>(
        &'a self,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<spot_permissions::model::Principal>> + Send + 'a>>;
}

/// Splits a validated room name into its doctype and optional document id, for the two
/// room kinds the permission world decides. Any other shape is `None` (the caller
/// refuses).
fn split_room(room: &Room) -> Option<(String, Option<String>)> {
    let name = room.as_str();
    if let Some(rest) = name.strip_prefix("doc:") {
        let (doctype, id) = rest.split_once(':')?;
        if doctype.is_empty() || id.is_empty() {
            return None;
        }
        return Some((doctype.to_owned(), Some(id.to_owned())));
    }
    if let Some(doctype) = name.strip_prefix("doctype:") {
        if doctype.is_empty() {
            return None;
        }
        return Some((doctype.to_owned(), None));
    }
    None
}

/// Whether `revalidate` may evict this room: every permission-decided room. The user room
/// and the site room are not permission questions (a notification room follows the user,
/// the site room carries site-wide notices), and an unknown future room shape is
/// evictable by default — fail closed.
fn is_evictable(room: &Room) -> bool {
    let name = room.as_str();
    !(name == Room::site().as_str() || name.starts_with("user:"))
}

/// The authorizer that decides every join over the permission core.
///
/// Clone-handle to the three injected seams; `RoomAuthorizer::authorize` runs their
/// futures **outside** any bus lock (the bus awaits the returned future before touching
/// its state — the concurrency rule of `spot-realtime`).
pub struct PermissionRoomAuthorizer<E: EngineSource, F: DocFactsSource, P: PrincipalSource> {
    engine: Arc<E>,
    facts: Arc<F>,
    principals: Arc<P>,
}

impl<E: EngineSource, F: DocFactsSource, P: PrincipalSource> Clone
    for PermissionRoomAuthorizer<E, F, P>
{
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            facts: Arc::clone(&self.facts),
            principals: Arc::clone(&self.principals),
        }
    }
}

impl<E: EngineSource, F: DocFactsSource, P: PrincipalSource> std::fmt::Debug
    for PermissionRoomAuthorizer<E, F, P>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionRoomAuthorizer")
            .finish_non_exhaustive()
    }
}

impl<E: EngineSource, F: DocFactsSource, P: PrincipalSource> PermissionRoomAuthorizer<E, F, P> {
    /// An authorizer over the three injected seams.
    pub fn new(engine: Arc<E>, facts: Arc<F>, principals: Arc<P>) -> Self {
        Self {
            engine,
            facts,
            principals,
        }
    }

    /// The rooms of `joined` the principal may no longer read under the **current**
    /// engine. The host calls this on every permission snapshot swap and has the
    /// principal's sessions leave the returned rooms.
    ///
    /// Why the host, not the bus, leaves: `spot-realtime` keeps room membership inside
    /// its subscriptions (one per connection) and publishes no membership list, by design
    /// — the host's session layer is where a subscription lives. This method is the
    /// decision; the session executes it with `Subscription::leave` and may then close or
    /// refetch.
    ///
    /// `user:` and `site` rooms are never returned (see [`is_evictable`]).
    ///
    /// A principal whose world cannot be loaded is treated as fully revoked: every
    /// evictable room is returned, which is the safe direction — staying joined without a
    /// verifiable permission would be the leak.
    pub async fn revalidate(
        &self,
        site: &SiteId,
        principal: &Principal,
        joined: &[Room],
    ) -> Vec<Room> {
        let Some(user) = self.permissions_principal(principal).await else {
            return joined
                .iter()
                .filter(|room| is_evictable(room))
                .cloned()
                .collect();
        };
        let mut evicted = Vec::new();
        for room in joined {
            if !is_evictable(room) || self.decide(site, &user, room).await.is_ok() {
                continue;
            }
            evicted.push(room.clone());
        }
        evicted
    }

    /// The permission world of a wire principal; `None` for a guest or an unknown user.
    async fn permissions_principal(
        &self,
        principal: &Principal,
    ) -> Option<spot_permissions::model::Principal> {
        if principal.is_guest() {
            return None;
        }
        self.principals.principal(principal.id()).await
    }

    /// The join decision for one room, given a resolved permission world. The error is
    /// the refusal's cause (logs only); `authorize` re-words every refusal identically.
    async fn decide(
        &self,
        site: &SiteId,
        user: &spot_permissions::model::Principal,
        room: &Room,
    ) -> Result<(), LookupFailure> {
        let name = room.as_str();
        if name == Room::site().as_str() {
            // Any signed-in non-guest (the guest refusal happened in `authorize`).
            let _ = site;
            return Ok(());
        }
        let Some((doctype, id)) = split_room(room) else {
            return Err(LookupFailure::UnknownDoctype);
        };
        let engine = self.engine.current();
        match id {
            // `doc:<dt>:<id>`: the engine's read decision on the document itself. Facts
            // that cannot be loaded refuse exactly like a genuine denial.
            Some(id) => {
                let Some(facts) = self.facts.facts(&doctype, &id).await else {
                    return Err(LookupFailure::UnknownDoctype);
                };
                if engine.can(user, Action::Read, &facts).allowed {
                    Ok(())
                } else {
                    Err(LookupFailure::UnknownDoctype)
                }
            }
            // `doctype:<dt>`: the read decision on a fact-less document — the "may read
            // the list" question.
            None => {
                let facts = DocFacts {
                    doctype: doctype.clone(),
                    ..DocFacts::default()
                };
                if engine.can(user, Action::Read, &facts).allowed {
                    Ok(())
                } else {
                    Err(LookupFailure::UnknownDoctype)
                }
            }
        }
    }
}

impl<E, F, P> spot_realtime::RoomAuthorizer for PermissionRoomAuthorizer<E, F, P>
where
    E: EngineSource + 'static,
    F: DocFactsSource + 'static,
    P: PrincipalSource + 'static,
{
    /// Authorizes one join.
    fn authorize(&self, site: &SiteId, principal: &Principal, room: &Room) -> AuthFuture<'_> {
        // The boxed future borrows only `self` (the elided output lifetime is `&self`'s).
        // The small owned inputs (a `SiteId`, a `Principal`, a `Room` — handle clones over
        // interned strings) are cloned here, before the `async move` block, so the block
        // captures owned values and one `&self` — the same shape as `AllowAll`'s
        // implementation in `spot-realtime`.
        let site = site.clone();
        let principal = principal.clone();
        let room = room.clone();
        Box::pin(async move {
            let user_room_self = room
                .as_str()
                .strip_prefix("user:")
                .is_some_and(|user| user == principal.id());
            if principal.is_guest() {
                // Guests are refused everything: no public rooms are declared yet.
                return Err(Denied::new(DENY_MESSAGE));
            }
            if user_room_self {
                // A user's own notification room: no permission world needed.
                return Ok(());
            }
            if room.as_str().starts_with("user:") {
                // Another user's notification room: not a permission the model expresses;
                // refused before the world is even built.
                return Err(Denied::new(DENY_MESSAGE));
            }
            let Some(user) = self.permissions_principal(&principal).await else {
                tracing::debug!(
                    code = LookupFailure::UnresolvedPrincipal.code(),
                    "room join refused at principal lookup"
                );
                return Err(Denied::new(DENY_MESSAGE));
            };
            match self.decide(&site, &user, &room).await {
                Ok(()) => Ok(()),
                Err(failure) => {
                    tracing::debug!(code = failure.code(), "room join refused at lookup");
                    Err(Denied::new(DENY_MESSAGE))
                }
            }
        })
    }
}
