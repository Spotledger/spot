//! The host intake: the two routes through which a peer's handshake message or revocation enters
//! a running platform (WP-CTL-10.T7; follow-ups of recorded security findings).
//!
//! * `POST /link/v1/handshake`: one `spot_tenant_link::HandshakeMessage` as JSON. Every inbound
//!   handshake message goes through [`LinkIntake::handshake`] **before** `Handshake::receive` or
//!   `from_proposal` sees it; the PostgreSQL implementation screens it with
//!   `PgLinkStore::claim_inbound` (signature, addressee or held link, peer key, then the nonce).
//! * `POST /link/v1/{link}/revoke`: one `spot_tenant_link::RevokeMessage` as JSON, applied through
//!   [`LinkIntake::revoke`] (`LinkAdmin::receive_revoke`: pinned signer, revision, nonce, state).
//!
//! # The order of checks
//!
//! 1. a client certificate must be present (`401`) and pinned to a party (`403`); both are
//!    answered from the directory alone, with no state kept and nothing verified;
//! 2. **the second-tier limiter** (L2): a token bucket keyed by that authenticated party, charged
//!    before the body is read and before any signature is verified. Over the limit the answer is
//!    `429` with `Retry-After`, and nothing is read, verified, claimed or audited; the only trace
//!    is one aggregated counter ([`IntakeCounters::limited`]). The key exists only for a pinned
//!    certificate, so an outsider cannot create keys; the table is still bounded by
//!    [`IntakeSettings::rate`] (`RateLimiter` drops idle full buckets first and refuses a new key
//!    when the table is full of busy ones). The existing per-link bucket (L1) is unchanged and
//!    runs on the message routes after the link is known;
//! 3. the body is read under its cap (`413`) and parsed (`400`);
//! 4. the message must belong to the caller: a proposal names the caller as its proposer, any
//!    other message or a revoke belongs to a link the caller is the peer of. An unknown link and
//!    somebody else's link answer identically (`403`);
//! 5. the port decides: `403` forged or wrong signer, `409` replayed or stale, `503` when a store
//!    cannot answer (the message is refused, never waved through).
//!
//! Which check failed is never told beyond those three shapes, and no key, signature, nonce or
//! body appears in an answer, a log line or an audit row of this crate.
//!
//! # Pruning
//!
//! Nonces older than the replay window are removed by [`LinkIntake::prune_nonces`]. The host calls
//! [`NonceSweep::run_if_due`] from its job scheduler with the instant and the `ConsentPolicy` it
//! runs the handshake under; this crate starts no thread or timer and reads no clock.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_tenant_link::{ConsentPolicy, HandshakeMessage, Party, RevokeMessage};

use crate::limits::RateConfig;

/// Why the intake refused a message. Deliberately coarse: it is all the peer is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntakeRefusal {
    /// The message does not verify, is signed by the wrong key, names a link or party it may not,
    /// or is otherwise not genuine (`403`).
    Forged,
    /// The message was seen before: its nonce was already used (`409`, `link_transport.replayed`).
    Replayed,
    /// The message is out of order or stale for the state of the link (`409`,
    /// `link_transport.invalid_request`).
    Stale,
    /// A store could not answer; nothing was accepted (`503`, retryable).
    Unavailable,
}

/// What the host brings to the intake routes. The routes call it from a blocking thread, so an
/// implementation may block on its stores.
///
/// The keys directory and the `Trust` that `claim_inbound` needs belong to the implementation:
/// they come from the host's handshake policy, never from a message.
pub trait LinkIntake: Send + Sync + 'static {
    /// Screens one inbound handshake message from the authenticated transport `peer` and, when it
    /// is genuine and new, claims its nonce and hands it on to the host's handshake. `at` is the
    /// instant in Unix milliseconds.
    ///
    /// # Errors
    /// [`IntakeRefusal`].
    fn handshake(
        &self,
        peer: &Party,
        message: &HandshakeMessage,
        at: u64,
    ) -> Result<(), IntakeRefusal>;

    /// Applies a revocation that arrived from the authenticated transport `peer`.
    ///
    /// # Errors
    /// [`IntakeRefusal`].
    fn revoke(&self, peer: &Party, message: &RevokeMessage, at: u64) -> Result<(), IntakeRefusal>;

    /// Deletes the nonces older than the replay window of `policy` and returns how many.
    ///
    /// # Errors
    /// [`IntakeRefusal::Unavailable`] when the store cannot answer.
    fn prune_nonces(&self, now: u64, policy: ConsentPolicy) -> Result<u64, IntakeRefusal>;
}

/// Limits of the intake routes.
#[derive(Clone, Copy, Debug)]
pub struct IntakeSettings {
    /// The second-tier bucket, one per authenticated peer, charged before anything is verified.
    pub rate: RateConfig,
    /// The largest handshake body (a proposal carries a draft, an offer and a consent).
    pub max_handshake_bytes: usize,
    /// The largest revoke body.
    pub max_revoke_bytes: usize,
}

impl Default for IntakeSettings {
    fn default() -> Self {
        Self {
            rate: RateConfig {
                burst: 10,
                per_second: 1,
                max_keys: 10_000,
            },
            max_handshake_bytes: 512 * 1024,
            max_revoke_bytes: 4 * 1024,
        }
    }
}

/// Counters of the intake routes, shared with the host for its health surface.
#[derive(Debug, Default)]
pub struct IntakeCounters {
    limited: AtomicU64,
}

impl IntakeCounters {
    /// How many requests the second-tier limiter cut off (the one aggregated refusal counter).
    #[must_use]
    pub fn limited(&self) -> u64 {
        self.limited.load(Ordering::Relaxed)
    }

    pub(crate) fn count_limited(&self) {
        self.limited.fetch_add(1, Ordering::Relaxed);
    }
}

/// Decides when the nonce prune is due. One function for the host's job scheduler: the interval
/// is configuration, the instant and the policy are arguments.
#[derive(Debug)]
pub struct NonceSweep {
    interval_ms: u64,
    last: Mutex<Option<u64>>,
}

impl NonceSweep {
    /// A sweep that runs at most once per `interval_ms` (at least one millisecond).
    #[must_use]
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval_ms: interval_ms.max(1),
            last: Mutex::new(None),
        }
    }

    /// Prunes through `intake` when the interval has passed since the last successful prune (the
    /// first call always prunes). Returns the number of nonces removed, or `None` when nothing was
    /// due. A failed prune is not recorded, so the next call tries again.
    ///
    /// # Errors
    /// [`IntakeRefusal::Unavailable`] when the store cannot answer.
    pub fn run_if_due(
        &self,
        intake: &dyn LinkIntake,
        now: u64,
        policy: ConsentPolicy,
    ) -> Result<Option<u64>, IntakeRefusal> {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let due = match *last {
            None => true,
            Some(then) => now < then || now - then >= self.interval_ms,
        };
        if !due {
            return Ok(None);
        }
        let removed = intake.prune_nonces(now, policy)?;
        *last = Some(now);
        Ok(Some(removed))
    }
}
