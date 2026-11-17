//! Realtime for one site: `GET /api/v1/realtime` over WebSocket, fed from the site's committed
//! change feed and outbox (ADR-25: notifications, never data).
//!
//! # Pieces
//!
//! - **The hub** ([`Realtime`]) is created once per site and survives a reload: the bus, the
//!   feed pump, the sockets' sessions and the route. Everything it asks of the running site
//!   (the permission snapshot, the engine, the sign-in service) it reads through [`SiteLive`] at
//!   the moment it asks, so a reload or a customisation swap reaches it without a rebuild.
//! - **Authentication** is the site's own sign-in service: the `auth` message's token is a
//!   session token or an API key, verified exactly like an HTTP bearer credential. A caller
//!   that has not yet chosen a password (a first sign-in) is refused, as everywhere else.
//! - **Rooms** are decided by [`PermissionRoomAuthorizer`] over three seams read from the live
//!   site: the published permission snapshot ([`LiveEngine`]), the committed facts of a
//!   document ([`LiveFacts`], loaded by the engine as the platform itself and mapped like the
//!   engine's own facts) and the principal behind a wire id ([`LivePrincipals`]: `None` for an
//!   unknown user or one that is not `active`).
//! - **The pump** reads the change feed and the outbox on a short interval and publishes room
//!   events. Its position is kept in memory: a restarted node begins at the start of the log,
//!   and no client is connected yet, so the replay only costs reads.
//! - **Revalidation.** When the permission snapshot is replaced, the rooms every connected
//!   principal has joined are asked again ([`PermissionRoomAuthorizer::revalidate`]). For each
//!   room whose answer is now "no" the host calls [`RealtimeBus::evict_room`]: the principal's
//!   connections stay open and in their other rooms (its own `user:` room included), and from
//!   the moment the call returns no further event of the evicted room reaches them, events
//!   already buffered included. The client may re-join, and is asked again.
//!
//!   A removed role assignment is a stored record and publishes no snapshot, so the service
//!   also watches the change feed: when a pump run has just fed an entry of a DocType that
//!   feeds the permission engine ([`is_authorisation_doctype`]), the joined rooms are asked
//!   again right after that run, once per run however many such entries it fed. A run that fed
//!   none of them asks nothing. Rooms are still asked again every few seconds
//!   ([`REVALIDATE_EVERY`]) as the backstop for anything this signal misses, for example a
//!   change feed that cannot be read.
//! - **Sessions.** A connection is closed (code 4009) only when its **session** ends: the
//!   transport asks the sessions again every few seconds, and a token that no longer verifies
//!   (expired, revoked, the user disabled) is rejected. A permission change never closes one.
//!
//! # What is never logged
//!
//! Tokens and event payloads. Sessions are keyed by a hash of the token, held in memory only.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::hash::BuildHasher;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::Name;
use spot_core::{Clock, SiteId};
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_http::api::codec::parse_record_id;
use spot_http::api::service::{
    Credential, Door, DoorTier, OfflineAdmission, OfflineRefusal, RequestCtx,
};
use spot_http::request::RequestId;
use spot_permissions::{DocFacts, PermissionEngine, Snapshot};
use spot_postgres::PgBackend;
use spot_realtime::ws::{self, CancellationToken, ExpiryWatch, Rejected, TokenVerifier, WsConfig};
use spot_realtime::{
    AuthFuture, BusConfig, Principal as WirePrincipal, RealtimeBus, Room, RoomAuthorizer,
};
use spot_realtime_bridge::{
    DocFactsSource, EngineSource, FeedPump, MappingOutcome, PermissionRoomAuthorizer,
    PositionStore, PrincipalSource as BridgePrincipals, PumpConfig, PumpPosition, PumpTelemetry,
};
use spot_storage_contract::{ChangeFeed, FeedPosition};
use tokio::sync::watch;

use crate::audit::PathClass;
use crate::live::{LiveProtection, SiteLive};
use crate::services::offline_identity::{
    self, EntryPoint, OfflineGate, OfflineIdentity, decide_at,
};
use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};

/// How often a connected session is asked whether it is still valid.
const SESSION_CHECK: Duration = Duration::from_secs(10);

/// How often every joined room is asked again even when nothing signalled a change: the
/// backstop for an authorisation change the change-feed signal missed (a role assignment is a
/// storage record that does not publish a snapshot; the pump normally revalidates on it at once).
const REVALIDATE_EVERY: Duration = Duration::from_secs(10);

/// Whether a change to a document of this DocType can change what a principal may read: the
/// records the permission engine's facts are loaded from. A change to one of these makes the
/// pump's run revalidate the joined rooms at once instead of waiting for [`REVALIDATE_EVERY`].
#[must_use]
pub fn is_authorisation_doctype(doctype: &str) -> bool {
    use spot_platform_app::doctype;
    matches!(
        doctype,
        doctype::ROLE
            | doctype::ROLE_DUTY
            | doctype::ROLE_ASSIGNMENT
            | doctype::ROLE_ASSIGNMENT_SCOPE
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// The permission engine of the snapshot published now.
pub struct LiveEngine {
    live: Arc<SiteLive>,
    /// Answers when no runtime is attached: nothing is granted.
    closed: PermissionEngine,
}

impl EngineSource for LiveEngine {
    fn current(&self) -> PermissionEngine {
        match self.live.served() {
            Some(served) => (*served.decision).clone(),
            None => self.closed.clone(),
        }
    }
}

/// The committed facts of a document, loaded as the platform and mapped like the engine's own.
pub struct LiveFacts {
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    site: SiteId,
}

impl LiveFacts {
    fn invocation(&self) -> Option<Invocation> {
        Some(Invocation {
            site: self.site.clone(),
            actor: Actor::System {
                component: Name::new("realtime").ok()?,
            },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        })
    }
}

impl DocFactsSource for LiveFacts {
    fn facts<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<DocFacts>> + Send + 'a>> {
        Box::pin(async move {
            let id = parse_record_id(id)?;
            let engine = self.live.engine()?;
            let inv = self.invocation()?;
            let loaded = engine.load(&inv, doctype, id).await.ok()?;
            let doc = loaded.value;
            Some(DocFacts {
                doctype: doc.doctype().as_str().to_owned(),
                id: (!doc.is_new()).then(|| doc.id().to_string()),
                created_by: doc
                    .fields()
                    .iter()
                    .find(|(name, _)| name.as_str() == "created_by")
                    .and_then(|(_, value)| match value {
                        spot_app_api::prelude::Value::Text(text) => Some(text.clone()),
                        _ => None,
                    }),
                state: doc.state().map(|s| s.as_str().to_owned()),
                values: doc
                    .fields()
                    .iter()
                    .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
                    .collect(),
            })
        })
    }
}

/// The permission principal behind a wire id.
pub struct LivePrincipals {
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    site: SiteId,
}

impl BridgePrincipals for LivePrincipals {
    fn principal<'a>(
        &'a self,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<spot_permissions::Principal>> + Send + 'a>> {
        Box::pin(async move {
            let record = parse_record_id(id)?;
            let engine = self.live.engine()?;
            let principals = self.live.principals()?;
            let inv = Invocation {
                site: self.site.clone(),
                actor: Actor::System {
                    component: Name::new("realtime").ok()?,
                },
                command_id: None,
                language: None,
                now: self.clock.now_utc(),
            };
            let user = engine.load(&inv, "user", record).await.ok()?.value;
            let active = matches!(
                user.fields().iter().find(|(n, _)| n.as_str() == "state"),
                Some((_, spot_app_api::prelude::Value::Text(state))) if state == "active"
            );
            if !active {
                return None;
            }
            Some(DocPrincipals::resolve(&*principals, &record).await)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Offline identity at the doors (WP-IAM-09.T5)
//
// The one place every door's plumbing lives (the coverage table in `offline_identity` names the
// file each door's decision is taken in; these types are what those files share).
// ---------------------------------------------------------------------------------------------

/// The node's monotonic reading for an offline decision (the clock the lease arithmetic uses,
/// never the wall clock). Provided by whatever holds the node's snapshot store.
pub trait NodeNow: Send + Sync + 'static {
    /// The reading now.
    fn now(&self) -> spot_offline_identity::NodeClock;
}

/// How long the node's lease for a tier still runs: the one fact a door with no user (the
/// management API) needs. Provided by whatever holds the node's snapshot store
/// (`SnapshotStore::lease_remaining_ms`).
pub trait LeaseProbe: Send + Sync + 'static {
    /// Milliseconds left on the lease of `tier`; zero when there is none.
    fn remaining_ms(&self, tier: DoorTier) -> u64;
}

/// [`OfflineAdmission`] over an [`OfflineGate`] (the node's snapshot store) and the node's clock:
/// every door's question becomes one [`decide_at`] call.
pub struct GateAdmission<G> {
    gate: G,
    now: Arc<dyn NodeNow>,
    lease: Option<Arc<dyn LeaseProbe>>,
}

impl<G: OfflineGate + Send + Sync + 'static> GateAdmission<G> {
    /// An admission over `gate`, reading `now` for every decision. Without
    /// [`GateAdmission::with_lease`] a door that asks for the node's own lease is `NotReady`.
    #[must_use]
    pub fn new(gate: G, now: Arc<dyn NodeNow>) -> Self {
        Self {
            gate,
            now,
            lease: None,
        }
    }

    /// The same admission, answering the node's own lease from `lease`.
    #[must_use]
    pub fn with_lease(mut self, lease: Arc<dyn LeaseProbe>) -> Self {
        self.lease = Some(lease);
        self
    }
}

/// The coverage-table entry a [`Door`] is.
#[must_use]
pub fn entry_point(door: Door) -> EntryPoint {
    match door {
        Door::Api => EntryPoint::Api,
        Door::Realtime => EntryPoint::Realtime,
        Door::Jobs => EntryPoint::Jobs,
        Door::Scripts => EntryPoint::Scripts,
        Door::Controllers => EntryPoint::Controllers,
        Door::Management => EntryPoint::Management,
    }
}

fn gate_tier(tier: DoorTier) -> spot_offline_identity::Tier {
    match tier {
        DoorTier::One => spot_offline_identity::Tier::One,
        DoorTier::Two => spot_offline_identity::Tier::Two,
        DoorTier::Three => spot_offline_identity::Tier::Three,
    }
}

impl<G: OfflineGate + Send + Sync + 'static> OfflineAdmission for GateAdmission<G> {
    fn admit(
        &self,
        door: Door,
        user: &str,
        tier: DoorTier,
        permission: Option<&str>,
    ) -> Result<(), OfflineRefusal> {
        let decision = decide_at(
            entry_point(door),
            &self.gate,
            user,
            gate_tier(tier),
            permission,
            self.now.now(),
        );
        if decision.is_allowed() {
            Ok(())
        } else {
            Err(OfflineRefusal::Denied)
        }
    }

    fn node_admitted(&self, _door: Door, tier: DoorTier) -> Result<(), OfflineRefusal> {
        let Some(lease) = &self.lease else {
            return Err(OfflineRefusal::NotReady);
        };
        if lease.remaining_ms(tier) > 0 {
            Ok(())
        } else {
            Err(OfflineRefusal::Denied)
        }
    }
}

/// The slot of `door` for `site`: the same one on every build of the site, so the controllers and
/// views the host builds once keep deciding through the admission a later reload installs.
#[must_use]
pub fn door_slot(site: &str, door: Door) -> Arc<OfflineSlot> {
    type Slots = Mutex<HashMap<(String, Door), Arc<OfflineSlot>>>;
    static SLOTS: std::sync::OnceLock<Slots> =
        std::sync::OnceLock::new();
    let mut slots = lock(SLOTS.get_or_init(|| Mutex::new(HashMap::new())));
    Arc::clone(
        slots
            .entry((site.to_owned(), door))
            .or_insert_with(|| Arc::new(OfflineSlot::default())),
    )
}

/// What one door found when its module finished registering.
#[derive(Clone)]
pub enum DoorState {
    /// The site issues no offline identity: the door is unchanged.
    Absent,
    /// The site issues offline identity but this node has no admission provider (no snapshot
    /// holder, so no node registry or user directory to decide with): the door reports not ready.
    Unwired,
    /// The door asks this decision before it acts.
    Enforcing(Arc<dyn OfflineAdmission>),
}

/// Reads a door's state from the assembly. Call it only once every module has registered (from a
/// finaliser, see [`finalise_door`]).
#[must_use]
pub fn door_state(ctx: &SiteCtx<'_>) -> DoorState {
    if let Some(admission) = ctx.port::<dyn OfflineAdmission>() {
        return DoorState::Enforcing(admission);
    }
    if ctx.port::<OfflineIdentity>().is_some() {
        return DoorState::Unwired;
    }
    DoorState::Absent
}

/// The readiness entry of a door that has no admission provider. It names the missing provider by
/// its error code and never a user, a node or a key.
#[must_use]
pub fn unwired_readiness(name: &'static str) -> Arc<Idle> {
    Idle::new(name, Level::Optional, || -> CheckFuture<'static> {
        Box::pin(async {
            Err(format!(
                "{}: this door has no offline admission provider (node registry, user directory)",
                offline_identity::CODE_NO_DIRECTORY
            ))
        })
    })
}

/// Runs `install` once every module has registered, with the admission when this door must
/// enforce one (`None` otherwise, which clears an earlier one after a reload); a door with no
/// provider while the site issues offline identity gets a not-ready entry named `name`.
pub fn finalise_door(
    ctx: &mut SiteCtx<'_>,
    name: &'static str,
    install: impl FnOnce(Option<Arc<dyn OfflineAdmission>>) + Send + 'static,
) {
    ctx.finally(move |ctx| {
        match door_state(ctx) {
            DoorState::Enforcing(admission) => install(Some(admission)),
            DoorState::Unwired => {
                install(None);
                ctx.add(unwired_readiness(name));
            }
            DoorState::Absent => install(None),
        }
        Ok(())
    });
}

/// A door's slot for its admission: empty means the door is unchanged. Replaceable, because a
/// reload builds a new assembly while the hub survives.
#[derive(Default)]
pub struct OfflineSlot {
    slot: Mutex<Option<Arc<dyn OfflineAdmission>>>,
}

impl OfflineSlot {
    /// Replaces (or clears) the admission.
    pub fn set(&self, admission: Option<Arc<dyn OfflineAdmission>>) {
        *lock(&self.slot) = admission;
    }

    /// Whether an admission is installed.
    #[must_use]
    pub fn enforcing(&self) -> bool {
        lock(&self.slot).is_some()
    }

    /// Asks the installed decision; an empty slot admits (the door is unchanged).
    ///
    /// # Errors
    /// The decision's refusal.
    pub fn admit(
        &self,
        door: Door,
        user: &str,
        tier: DoorTier,
        permission: Option<&str>,
    ) -> Result<(), OfflineRefusal> {
        let admission = lock(&self.slot).clone();
        match admission {
            Some(admission) => admission.admit(door, user, tier, permission),
            None => Ok(()),
        }
    }

    /// Asks whether the node itself holds a valid lease; an empty slot admits.
    ///
    /// # Errors
    /// The decision's refusal.
    pub fn admit_node(&self, door: Door, tier: DoorTier) -> Result<(), OfflineRefusal> {
        let admission = lock(&self.slot).clone();
        match admission {
            Some(admission) => admission.node_admitted(door, tier),
            None => Ok(()),
        }
    }

    /// Asks the decision for the person behind `actor`: a user, and the person who acted as that
    /// user when there is one (impersonation); either refused refuses. A platform component acts
    /// for nobody and a consumer is a separate principal outside the offline directory (ADR-083):
    /// neither is decided here.
    ///
    /// # Errors
    /// The first refusal.
    pub fn admit_actor(
        &self,
        door: Door,
        actor: &Actor,
        tier: DoorTier,
        permission: Option<&str>,
    ) -> Result<(), OfflineRefusal> {
        let Actor::User { id, via } = actor else {
            return Ok(());
        };
        for user in std::iter::once(id).chain(via.as_ref()) {
            self.admit(door, &user.to_string(), tier, permission)?;
        }
        Ok(())
    }
}

/// The app error a refusal becomes: `permission.denied`, or `server.unavailable` (retry) when the
/// decision could not be taken. Neither names a user, a permission or a node.
#[must_use]
pub fn refusal_error(refusal: OfflineRefusal) -> spot_app_api::AppError {
    match refusal {
        OfflineRefusal::Denied => spot_app_api::AppError::new(
            spot_app_api::ErrorClass::PermissionDenied,
            "permission.denied",
            "The identity is not admitted on this node.",
        ),
        OfflineRefusal::NotReady => spot_app_api::AppError::new(
            spot_app_api::ErrorClass::Unavailable,
            "server.unavailable",
            "The node cannot decide the identity yet.",
        ),
    }
}

type Authorizer = PermissionRoomAuthorizer<LiveEngine, LiveFacts, LivePrincipals>;

/// The rooms each principal has been admitted to, so a snapshot swap knows what to ask again.
#[derive(Default)]
struct Ledger {
    rooms: Mutex<HashMap<String, BTreeSet<String>>>,
}

impl Ledger {
    fn record(&self, user: &str, room: &Room) {
        lock(&self.rooms)
            .entry(user.to_owned())
            .or_default()
            .insert(room.as_str().to_owned());
    }

    fn forget_user(&self, user: &str) {
        lock(&self.rooms).remove(user);
    }

    fn forget_rooms(&self, user: &str, rooms: &[Room]) {
        let mut all = lock(&self.rooms);
        if let Some(joined) = all.get_mut(user) {
            for room in rooms {
                joined.remove(room.as_str());
            }
        }
    }

    fn snapshot(&self) -> Vec<(String, Vec<Room>)> {
        lock(&self.rooms)
            .iter()
            .map(|(user, rooms)| {
                (
                    user.clone(),
                    rooms.iter().filter_map(|r| Room::parse(r).ok()).collect(),
                )
            })
            .collect()
    }
}

/// The bus's authorizer: the permission rules, and a note of every admission.
struct Ledgered {
    inner: Authorizer,
    ledger: Arc<Ledger>,
    offline: Arc<OfflineSlot>,
}

impl RoomAuthorizer for Ledgered {
    fn authorize(&self, site: &SiteId, principal: &WirePrincipal, room: &Room) -> AuthFuture<'_> {
        let (site, principal, room) = (site.clone(), principal.clone(), room.clone());
        Box::pin(async move {
            // The offline decision first: an identity revoked offline joins no room.
            self.offline
                .admit(Door::Realtime, principal.id(), DoorTier::Three, None)
                .map_err(|_| spot_realtime::Denied::new("the identity is not admitted"))?;
            self.inner.authorize(&site, &principal, &room).await?;
            self.ledger.record(principal.id(), &room);
            Ok(())
        })
    }
}

/// The sockets' sessions: token verification, and the means to close a principal's connections.
pub struct Sessions {
    live: Arc<SiteLive>,
    ledger: Arc<Ledger>,
    keys: std::collections::hash_map::RandomState,
    /// The user a token verified as, until its connection's watch picks it up.
    pending: Mutex<HashMap<u64, String>>,
    /// The live connections of each user.
    watchers: Mutex<HashMap<String, Vec<Weak<ConnWatch>>>>,
    me: Weak<Sessions>,
    /// The offline decision, asked at every verification (the transport repeats it every
    /// [`SESSION_CHECK`], so a lease boundary or a revocation closes the socket).
    offline: Arc<OfflineSlot>,
}

impl Sessions {
    fn key(&self, token: &str) -> u64 {
        self.keys.hash_one(token)
    }

    /// How many sockets are connected for `user`.
    pub fn connections(&self, user: &str) -> usize {
        lock(&self.watchers).get(user).map_or(0, |list| {
            list.iter().filter(|w| w.strong_count() > 0).count()
        })
    }
}

impl TokenVerifier for Sessions {
    fn verify<'a>(
        &'a self,
        site: &'a SiteId,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<WirePrincipal, Rejected>> + Send + 'a>> {
        Box::pin(async move {
            let key = self.key(token);
            let auth = self
                .live
                .auth()
                .ok_or_else(|| Rejected::new("the site is not serving"))?;
            let ctx = RequestCtx {
                site: site.clone(),
                credential: Some(Credential::new(token)),
                idempotency_key: None,
                language: None,
                request_id: RequestId::generate(),
                peer: None,
            };
            // A refused API key is audited as presented to the realtime socket.
            let who = auth
                .authenticate_on(&ctx, PathClass::Realtime)
                .await
                .map_err(|_| Rejected::new("the credential is not valid"))?;
            if auth.is_restricted(&who) {
                return Err(Rejected::new("the user must choose a password first"));
            }
            let user = who.user_id.to_string();
            self.offline
                .admit(Door::Realtime, &user, DoorTier::Three, None)
                .map_err(|_| Rejected::new("the credential is not valid"))?;
            lock(&self.pending).insert(key, user.clone());
            Ok(WirePrincipal::user(user))
        })
    }

    fn expiry_watch(&self, token: &str) -> Option<Arc<dyn ExpiryWatch>> {
        let key = self.key(token);
        let user = lock(&self.pending).remove(&key)?;
        let watch = Arc::new(ConnWatch {
            sessions: self.me.upgrade()?,
            user: user.clone(),
            key,
        });
        let mut watchers = lock(&self.watchers);
        let list = watchers.entry(user).or_default();
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(&watch));
        Some(watch)
    }
}

/// One connection's watch: it resolves on a timer, so the transport asks the sessions again
/// whether the connection's token still verifies (and closes the connection when it does not).
struct ConnWatch {
    sessions: Arc<Sessions>,
    user: String,
    key: u64,
}

impl ExpiryWatch for ConnWatch {
    fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(SESSION_CHECK))
    }
}

impl Drop for ConnWatch {
    fn drop(&mut self) {
        lock(&self.sessions.pending).remove(&self.key);
        let mut watchers = lock(&self.sessions.watchers);
        if let Some(list) = watchers.get_mut(&self.user) {
            // This watch's own strong count is already zero here.
            list.retain(|w| w.strong_count() > 0);
            if list.is_empty() {
                watchers.remove(&self.user);
                self.sessions.ledger.forget_user(&self.user);
            }
        }
    }
}

/// The pump's position, in memory.
#[derive(Default)]
struct MemoryPosition(Mutex<Option<PumpPosition>>);

impl PositionStore for MemoryPosition {
    fn load(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<PumpPosition>, String>> + Send + '_>> {
        Box::pin(async move { Ok(lock(&self.0).clone()) })
    }

    fn save<'a>(
        &'a self,
        position: &'a PumpPosition,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            *lock(&self.0) = Some(position.clone());
            Ok(())
        })
    }
}

/// The pump's counters, as log lines at debug level.
struct PumpLog {
    site: SiteId,
}

impl PumpTelemetry for PumpLog {
    fn record_run(&self, outcome: &MappingOutcome) {
        if outcome.room_events > 0 {
            tracing::debug!(site = %self.site, events = outcome.room_events, "realtime events published");
        }
    }

    fn record_lag(&self, log: &'static str, entries_behind: u64) {
        tracing::debug!(site = %self.site, log, entries_behind, "realtime pump lag");
    }

    fn record_resync(&self, log: &'static str) {
        tracing::warn!(site = %self.site, log, "realtime pump position lost; clients resync");
    }
}

/// Settings of a site's realtime hub.
pub struct Settings {
    /// The site.
    pub site: SiteId,
    /// What the site serves right now.
    pub live: Arc<SiteLive>,
    /// The site's database: the feed the pump reads.
    pub backend: PgBackend,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
    /// Seconds between WebSocket pings.
    pub heartbeat_seconds: u64,
    /// How often the pump reads the feed.
    pub poll: Duration,
}

/// The realtime hub of one site.
pub struct Realtime {
    site: SiteId,
    live: Arc<SiteLive>,
    poll: Duration,
    router: axum::Router,
    sessions: Arc<Sessions>,
    authorizer: Authorizer,
    ledger: Arc<Ledger>,
    pump: FeedPump<PgBackend, PumpLog>,
    /// The feed the pump reads, read again (never written) to see which DocTypes a run fed.
    feed: Arc<PgBackend>,
    page_limit: u32,
    stop_sockets: CancellationToken,
    bus: RealtimeBus,
    offline: Arc<OfflineSlot>,
}

impl Realtime {
    /// Builds the hub: the bus, the route, the sessions and the pump. `None` when the site has
    /// no runtime attached yet (there is nothing to decide rooms with).
    ///
    /// Nothing is read or published until [`Realtime::service`] runs.
    pub fn new(settings: Settings) -> Option<Arc<Self>> {
        let Settings {
            site,
            live,
            backend,
            clock,
            heartbeat_seconds,
            poll,
        } = settings;
        // The engine rooms are decided with while no snapshot is published: it grants nothing.
        let closed = PermissionEngine::new(Arc::new(Snapshot::empty(&live.served()?.meta).ok()?));
        let authorizer = PermissionRoomAuthorizer::new(
            Arc::new(LiveEngine {
                live: Arc::clone(&live),
                closed,
            }),
            Arc::new(LiveFacts {
                live: Arc::clone(&live),
                clock: Arc::clone(&clock),
                site: site.clone(),
            }),
            Arc::new(LivePrincipals {
                live: Arc::clone(&live),
                clock: Arc::clone(&clock),
                site: site.clone(),
            }),
        );
        let ledger = Arc::new(Ledger::default());
        let offline = Arc::new(OfflineSlot::default());
        let bus = RealtimeBus::new(
            BusConfig::default(),
            Arc::clone(&clock),
            Arc::new(Ledgered {
                inner: authorizer.clone(),
                ledger: Arc::clone(&ledger),
                offline: Arc::clone(&offline),
            }),
        );
        let stop_sockets = CancellationToken::new();
        let sessions = Arc::new_cyclic(|me| Sessions {
            live: Arc::clone(&live),
            ledger: Arc::clone(&ledger),
            keys: std::collections::hash_map::RandomState::new(),
            pending: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            me: me.clone(),
            offline: Arc::clone(&offline),
        });
        let router = ws::router(
            bus.clone(),
            sessions.clone(),
            WsConfig {
                heartbeat_seconds,
                shutdown: stop_sockets.clone(),
            },
        );
        let feed = Arc::new(backend);
        let config = PumpConfig::default();
        let page_limit = config.page_limit;
        let pump = FeedPump::new(
            site.clone(),
            Arc::clone(&feed),
            bus.clone(),
            Arc::new(MemoryPosition::default()),
            None,
            Arc::new(PumpLog { site: site.clone() }),
            config,
        )
        // A protected field name never reaches a room, and a value-bearing notice is refused
        // (WP-SEC-04.T8, installed by WP-SEC-04.T14). The facts are read from what is served at
        // the moment of each event, so a customisation swapped in later applies at once.
        .with_notification_guard(Arc::new(LiveProtection::new(Arc::clone(&live))));
        Some(Arc::new(Realtime {
            site,
            live,
            poll,
            router,
            sessions,
            authorizer,
            ledger,
            pump,
            feed,
            page_limit,
            stop_sockets,
            bus,
            offline,
        }))
    }

    /// Installs (or, with `None`, clears) the offline admission every join, verification and
    /// revalidation asks (WP-IAM-09.T5). Called from the finaliser of [`register`].
    pub fn set_offline_admission(&self, admission: Option<Arc<dyn OfflineAdmission>>) {
        self.offline.set(admission);
    }

    /// The slot the sockets' decisions read (for tests).
    #[must_use]
    pub fn offline_slot(&self) -> &Arc<OfflineSlot> {
        &self.offline
    }

    /// The `GET /api/v1/realtime` route.
    pub fn router(&self) -> axum::Router {
        self.router.clone()
    }

    /// The bus the pump publishes to (for tests and metrics).
    pub fn bus(&self) -> &RealtimeBus {
        &self.bus
    }

    /// The sockets' sessions (for tests).
    pub fn sessions(&self) -> &Arc<Sessions> {
        &self.sessions
    }

    /// The pump and revalidation as a hosted service.
    pub fn service(self: &Arc<Self>) -> Arc<dyn Service> {
        Arc::new(RealtimeService {
            hub: Arc::clone(self),
        })
    }

    /// Runs the pump once. When the run fed a change to an authorisation record
    /// ([`is_authorisation_doctype`]), asks the joined rooms again right away, once.
    ///
    /// `fed_up_to` is the change-feed position the previous run ended at (the pump's own
    /// position, the start of the log before the first run).
    async fn pump_and_revalidate(
        &self,
        fed_up_to: &mut FeedPosition,
    ) -> Result<(), spot_realtime_bridge::BridgeError> {
        let outcome = self.pump.pump_once().await?;
        let before = std::mem::replace(fed_up_to, outcome.position.changes);
        let fed = outcome.mapping.changes_published > 0;
        if !outcome.mapping.changes_resynced && !fed {
            return Ok(());
        }
        // Nobody joined: nothing to ask, and a replay after a restart costs no second read.
        if self.ledger.snapshot().is_empty() {
            return Ok(());
        }
        // A resync lost entries we cannot see; so does a feed that cannot be read again.
        let relevant = if outcome.mapping.changes_resynced {
            true
        } else {
            match self.feed.read_changes(before, self.page_limit).await {
                Ok(changes) => changes.iter().any(|change| {
                    change.position <= outcome.position.changes
                        && is_authorisation_doctype(change.document.entity.as_str())
                }),
                Err(_) => true,
            }
        };
        if relevant {
            self.revalidate().await;
        }
        Ok(())
    }

    /// Asks again about every room a connected principal joined, under the snapshot published
    /// now, and takes the principal out of each room it may no longer read. Connections stay
    /// open; only the rooms go.
    pub async fn revalidate(&self) {
        for (user, rooms) in self.ledger.snapshot() {
            if self.sessions.connections(&user) == 0 {
                self.ledger.forget_user(&user);
                continue;
            }
            let principal = WirePrincipal::user(user.clone());
            // A user the offline decision no longer admits keeps no room (the socket itself
            // closes at its next verification).
            let evicted = if self
                .offline
                .admit(Door::Realtime, &user, DoorTier::Three, None)
                .is_err()
            {
                rooms.clone()
            } else {
                self.authorizer
                    .revalidate(&self.site, &principal, &rooms)
                    .await
            };
            if evicted.is_empty() {
                continue;
            }
            // Forget first: a join that races this call is admitted (and recorded) again.
            self.ledger.forget_rooms(&user, &evicted);
            let mut left = 0;
            for room in &evicted {
                left += self.bus.evict_room(&self.site, &principal, room);
            }
            tracing::info!(
                site = %self.site,
                rooms = evicted.len(),
                subscriptions = left,
                "a permission change took rooms away from a connected principal"
            );
        }
    }
}

struct RealtimeService {
    hub: Arc<Realtime>,
}

impl Service for RealtimeService {
    fn name(&self) -> &'static str {
        "realtime"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.hub
                .pump
                .pump_once()
                .await
                .map(|_| ())
                .map_err(|e| format!("the change feed cannot be read: {e}"))
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut swaps = self.hub.live.swaps();
            swaps.mark_unchanged();
            let mut tick = tokio::time::interval(self.hub.poll);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut again = tokio::time::interval(REVALIDATE_EVERY);
            again.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            again.reset();
            let mut failing = false;
            let mut fed_up_to = FeedPosition::START;
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            self.hub.stop_sockets.cancel();
                            return Ok(());
                        }
                    }
                    changed = swaps.changed() => {
                        if changed.is_err() {
                            continue;
                        }
                        self.hub.revalidate().await;
                    }
                    _ = again.tick() => {
                        self.hub.revalidate().await;
                    }
                    _ = tick.tick() => {
                        match self.hub.pump_and_revalidate(&mut fed_up_to).await {
                            Ok(()) => failing = false,
                            Err(e) if !failing => {
                                failing = true;
                                tracing::warn!(site = %self.hub.site, %e, "the realtime pump failed; retrying");
                            }
                            Err(_) => {}
                        }
                    }
                }
            }
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            self.hub.stop_sockets.cancel();
            Ok(())
        })
    }
}

/// This module's start order: the one it had in the host's table before it moved here.
pub const ORDER: u32 = 800;

/// Realtime: optional, so a feed that cannot be read degrades the site, not stops it.
pub async fn register(ctx: &mut SiteCtx<'_>) -> crate::Result<()> {
    if let Some(hub) = ctx.env()?.live.realtime() {
        ctx.add(hub.service());
        let door = Arc::clone(&hub);
        finalise_door(ctx, "offline_door_realtime", move |admission| {
            door.set_offline_admission(admission);
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_authorisation_doctype;

    #[test]
    fn only_the_records_the_permission_engine_reads_signal_a_revalidation() {
        for doctype in [
            "role",
            "role_duty",
            "role_assignment",
            "role_assignment_scope",
        ] {
            assert!(is_authorisation_doctype(doctype), "{doctype}");
        }
        for doctype in [
            "task",
            "project",
            "user",
            "session",
            "api_key",
            "sys_user_notice",
        ] {
            assert!(!is_authorisation_doctype(doctype), "{doctype}");
        }
    }
}
