//! The offline identity snapshot keeper: the signed permission snapshot an edge may believe
//! while offline, kept until its lease expires.
//!
//! The rules are the library's ([`SnapshotStore`]): an unsigned, expired, rolled-back, forked or
//! replayed snapshot is refused; the lease runs on the node's monotonic clock; a restart on
//! another boot charges the unmeasured gap and never extends the lease; every missing fact is a
//! denial. This module adds the host's three things and invents no rule:
//!
//! * [`SnapshotKeeper::accept`] also refuses a message that arrives while the node's monotonic
//!   clock reads **lower than the newest reading this keeper saw on the same boot** (a clock
//!   moved back), before the store looks at it. The store then fails closed on its own too
//!   (`offline_identity.deny.clock_fault`), so a moved-back clock extends nothing.
//! * the request the carrier sends ([`SnapshotKeeper::request`]): the node's report of what it
//!   holds, so the hub can see a replayed image;
//! * the readiness figures ([`SnapshotKeeper::standing`], [`SnapshotKeeper::snapshot_age`]).
//!
//! The persistence is a type parameter: a sealed one
//! ([`spot_offline_identity_adapters::SealedPersistence`] over [`crate::identity_pg::PgIdentityState`])
//! in production. The offline root key never exists on an edge; the keeper holds only the root
//! **public** key (the [`TrustAnchor`]).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

pub use spot_offline_identity::NodeClock;
use spot_offline_identity::signing::{RequestKind, sign_node_request};
use spot_offline_identity::{
    Accepted, AcceptedKind, Decision, Message, Persistence, Received, Rejection, Signer,
    SnapshotStore, Tier, TrustAnchor,
};

use crate::carrier::SnapshotMsg;
use crate::lease::Standing;

/// The stable code of a message refused because the clock moved back.
pub const CLOCK_MOVED_BACK: &str = "edge_offline.clock_moved_back";

/// Why a message was not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeeperError {
    /// The monotonic clock read lower than before on the same boot.
    ClockMovedBack,
    /// The store refused the message; the library's rejection says exactly why.
    Rejected(Rejection),
}

impl KeeperError {
    /// The stable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ClockMovedBack => CLOCK_MOVED_BACK,
            Self::Rejected(rejection) => rejection.code(),
        }
    }
}

impl fmt::Display for KeeperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockMovedBack => write!(
                f,
                "{CLOCK_MOVED_BACK}: the node's clock moved back; the message was not accepted"
            ),
            Self::Rejected(rejection) => write!(f, "{rejection}"),
        }
    }
}

impl std::error::Error for KeeperError {}

/// A [`Persistence`] behind a box, so one keeper type serves any backend.
pub struct BoxedPersistence(pub Box<dyn Persistence + Send>);

impl Persistence for BoxedPersistence {
    fn load(
        &self,
    ) -> Result<Option<spot_offline_identity::Persisted>, spot_offline_identity::PersistError> {
        self.0.load()
    }

    fn save(
        &mut self,
        state: &spot_offline_identity::Persisted,
    ) -> Result<(), spot_offline_identity::PersistError> {
        self.0.save(state)
    }

    fn nonce_floor(&self) -> u64 {
        self.0.nonce_floor()
    }
}

/// What a module that holds the sealing key hands the edge so the keeper can be opened: the
/// trust anchor (tenant, node and the offline **root public key**) and the sealed persistence.
/// The persistence is taken once, at start-up.
pub struct IdentityParts {
    /// The anchor.
    pub anchor: TrustAnchor,
    /// The node's own signing key (behind the trait: the key never leaves its holder). It signs
    /// the snapshot requests the hub verifies.
    pub node_key: Arc<dyn Signer + Send + Sync>,
    /// The persistence, until the keeper takes it.
    pub persistence: std::sync::Mutex<Option<BoxedPersistence>>,
}

impl IdentityParts {
    /// Parts over `persistence`.
    pub fn new(
        anchor: TrustAnchor,
        node_key: Arc<dyn Signer + Send + Sync>,
        persistence: BoxedPersistence,
    ) -> Self {
        Self {
            anchor,
            node_key,
            persistence: std::sync::Mutex::new(Some(persistence)),
        }
    }

    /// Opens the keeper at `now`; `None` when the persistence was already taken.
    pub fn open(&self, now: NodeClock) -> Option<SnapshotKeeper<BoxedPersistence>> {
        let persistence = self
            .persistence
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()?;
        Some(
            SnapshotKeeper::open(self.anchor.clone(), persistence, now)
                .with_node_key(Arc::clone(&self.node_key)),
        )
    }
}

/// The keeper.
pub struct SnapshotKeeper<P: Persistence> {
    store: SnapshotStore<P>,
    newest: NodeClock,
    node_key: Option<Arc<dyn Signer + Send + Sync>>,
}

impl<P: Persistence> SnapshotKeeper<P> {
    /// Opens the keeper at `now`, restoring what the persistence holds (the store's own rules: an
    /// unreadable or unverifiable state leaves no snapshot and keeps the epoch counters).
    pub fn open(anchor: TrustAnchor, persistence: P, now: NodeClock) -> Self {
        Self {
            store: SnapshotStore::open(anchor, persistence, now),
            newest: now,
            node_key: None,
        }
    }

    /// Gives the keeper the node's signing key, so it can sign the requests it builds.
    #[must_use]
    pub fn with_node_key(mut self, key: Arc<dyn Signer + Send + Sync>) -> Self {
        self.node_key = Some(key);
        self
    }

    /// The library store, for the checks it owns (`sign_in`, `authorise_*`, overrides).
    pub fn store(&self) -> &SnapshotStore<P> {
        &self.store
    }

    /// The library store, mutably (certificate renewal, override policy).
    pub fn store_mut(&mut self) -> &mut SnapshotStore<P> {
        &mut self.store
    }

    fn observe(&mut self, now: NodeClock) -> Result<(), KeeperError> {
        if now.boot_id == self.newest.boot_id && now.mono_ms < self.newest.mono_ms {
            return Err(KeeperError::ClockMovedBack);
        }
        if now.boot_id != self.newest.boot_id || now.mono_ms > self.newest.mono_ms {
            self.newest = now;
        }
        Ok(())
    }

    /// Accepts a snapshot, a timestamp or a signed hub time the carrier delivered.
    ///
    /// # Errors
    ///
    /// [`KeeperError::ClockMovedBack`] or the store's [`Rejection`] (nothing changes).
    pub fn accept(&mut self, message: &Message, now: NodeClock) -> Result<Accepted, KeeperError> {
        self.observe(now)?;
        self.store
            .accept(message, Received { clock: now })
            .map_err(KeeperError::Rejected)
    }

    /// Whether `user` may sign in at `now`. A clock that moved back denies.
    pub fn sign_in(&mut self, user: &str, now: NodeClock) -> Decision {
        if self.observe(now).is_err() {
            return Decision::Deny(spot_offline_identity::DenyReason::ClockFault);
        }
        self.store.sign_in(user, now)
    }

    /// Whether `user` may act at `tier` holding `permission` at `now`. A clock that moved back
    /// denies.
    pub fn authorise_action(
        &mut self,
        user: &str,
        tier: Tier,
        permission: &str,
        now: NodeClock,
    ) -> Decision {
        if self.observe(now).is_err() {
            return Decision::Deny(spot_offline_identity::DenyReason::ClockFault);
        }
        self.store.authorise_action(user, tier, permission, now)
    }

    /// The signed request the carrier sends to ask for a fresh snapshot or timestamp: the
    /// node's report of what it holds, signed with the node key. `None` when the keeper holds
    /// no node key (nothing is sent unsigned: the hub would refuse it).
    pub fn request(&self, request_id: impl Into<String>) -> Option<SnapshotMsg> {
        let key = self.node_key.as_ref()?;
        let request = self.store.node_request(RequestKind::Snapshot);
        Some(SnapshotMsg::Request {
            request_id: request_id.into(),
            signed: sign_node_request(request, key.as_ref()),
        })
    }

    /// The id the next request carries: the epoch held and the newest nonce, so a resend before
    /// an answer is idempotent and a request after an answer is new.
    pub fn next_request_id(&self) -> String {
        let report = self.store.report();
        format!("snapshot:{}@{}", report.highest_epoch, report.hub_nonce)
    }

    /// Checkpoints the lease (call at least every `MAX_CHECKPOINT_GAP`).
    ///
    /// # Errors
    ///
    /// [`KeeperError::Rejected`] with `PersistFailed` when the state is not durably stored.
    pub fn checkpoint(&mut self, now: NodeClock) -> Result<(), KeeperError> {
        self.observe(now)?;
        self.store.checkpoint(now).map_err(KeeperError::Rejected)
    }

    /// The identity lease's standing: the time left of the ordinary tier's lease (tier 3, the
    /// longest), or lapsed. With no snapshot at all it is lapsed.
    pub fn standing(&self, now: NodeClock) -> Standing {
        if self.store.snapshot().is_none() {
            return Standing::Lapsed;
        }
        match self.store.lease_remaining_ms(Tier::Three, now) {
            0 => Standing::Lapsed,
            ms => Standing::Alive(Duration::from_millis(ms)),
        }
    }

    /// How long the snapshot has been running: the lease of tier 3 as issued minus what is left
    /// of it. `None` with no snapshot. It survives a restart (the lease does).
    pub fn snapshot_age(&self, now: NodeClock) -> Option<Duration> {
        let snapshot = self.store.snapshot()?;
        let deadlines = self.store.deadlines()?;
        let total = deadlines
            .get(Tier::Three)
            .saturating_sub(snapshot.issued_at);
        let left = self.store.lease_remaining_ms(Tier::Three, now);
        Some(Duration::from_millis(total.saturating_sub(left)))
    }

    /// Whether the keeper should ask the hub for a refresh: no snapshot, or less than a quarter
    /// of the lease left.
    pub fn refresh_due(&self, now: NodeClock) -> bool {
        let (Some(snapshot), Some(deadlines)) = (self.store.snapshot(), self.store.deadlines())
        else {
            return true;
        };
        let total = deadlines
            .get(Tier::Three)
            .saturating_sub(snapshot.issued_at);
        let left = self.store.lease_remaining_ms(Tier::Three, now);
        left.saturating_mul(4) < total
    }

    /// The kind of the last message is not kept; this names what an accepted message was.
    pub fn describe(kind: AcceptedKind) -> &'static str {
        match kind {
            AcceptedKind::Snapshot => "snapshot",
            AcceptedKind::Timestamp => "timestamp",
            AcceptedKind::HubTime => "hub_time",
        }
    }
}
