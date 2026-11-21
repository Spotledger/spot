//! The durable log of used handshake nonces.
//!
//! A [`Handshake`](crate::handshake::Handshake) keeps its used nonces in memory only, so a
//! process that restarts and rebuilds the handshake from the stored link would forget every
//! nonce and could accept a captured message a second time. Every send and every receive
//! therefore records its nonce through this port **before** it acts; the in-memory set in the
//! handshake is a cache, not the record.
//!
//! The PostgreSQL implementation (WP-CTL-09.T2) records through a unique constraint on
//! `(link_id, nonce)` and returns `false` on a conflict, so one row per nonce is the durable
//! answer to "was this message already used on this link?". The trait is the whole contract:
//! this crate never opens a connection.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::ids::{LinkId, Nonce};

const CLAIM_OPEN: u8 = 0;
const CLAIM_USED: u8 = 1;
const CLAIM_RELEASED: u8 = 2;

/// Proof that the caller itself just claimed one nonce on one link in a nonce log, for the one
/// message it is handling (WP-CTL-10.T8).
///
/// The host intake claims a message's nonce before it hands the message to the handshake (so a
/// message that is not genuine can never add a row). The handshake would then record the same
/// nonce again and read its own caller's claim as a replay. A claim lets the handshake say "this
/// nonce is the one my caller claimed" through [`NonceLog::record_claimed`], instead of making
/// `record` idempotent for everyone, which would turn every replay into a success.
///
/// A claim:
/// * names one link and one nonce; a log refuses it for any other;
/// * is spent at most once: the first [`NonceLog::record_claimed`] that matches wins, every later
///   one (also through a copy, which shares the state) is refused;
/// * is dead after [`release`](Self::release): the intake releases it when it gives the nonce back
///   because the message was not processed, and a released claim is never accepted.
///
/// The mint is sealed (WP-CTL-09.T7): `NonceClaim::granted` is crate private, there is no
/// `Default`, `Deserialize` or `From`, so outside this crate a claim comes only from [`claim_with`]
/// (or [`claim_with_async`]) over a value whose type implements the store primitive
/// ([`NonceClaimer`], [`AsyncNonceClaimer`]) and reports that it just inserted the nonce. The logs
/// additionally check that the nonce is really held.
///
/// ```compile_fail
/// use spot_tenant_link::{LinkId, Nonce, NonceClaim};
/// # fn forge(link: LinkId, nonce: Nonce) {
/// let _forged = NonceClaim::granted(link, nonce);
/// # }
/// ```
#[derive(Clone)]
pub struct NonceClaim {
    link: LinkId,
    nonce: Nonce,
    state: Arc<AtomicU8>,
}

impl std::fmt::Debug for NonceClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NonceClaim")
            .field("link", &self.link)
            .finish_non_exhaustive()
    }
}

impl NonceClaim {
    /// A claim for `nonce` on `link`. Crate private: it is built only by [`claim_with`] and
    /// [`claim_with_async`], right after a store's atomic insert reported the nonce as new.
    #[must_use]
    pub(crate) fn granted(link: LinkId, nonce: Nonce) -> Self {
        Self {
            link,
            nonce,
            state: Arc::new(AtomicU8::new(CLAIM_OPEN)),
        }
    }

    /// The link this claim is for.
    #[must_use]
    pub fn link(&self) -> &LinkId {
        &self.link
    }

    /// Whether this claim is for exactly `nonce` on `link`.
    #[must_use]
    pub fn is_for(&self, link: &LinkId, nonce: &Nonce) -> bool {
        self.link == *link && self.nonce == *nonce
    }

    /// Spends the claim for `nonce` on `link`. True only the first time, only for the claim's own
    /// link and nonce, and never after [`release`](Self::release). A refused attempt leaves the
    /// claim as it was.
    #[must_use]
    pub fn spend(&self, link: &LinkId, nonce: &Nonce) -> bool {
        self.is_for(link, nonce)
            && self
                .state
                .compare_exchange(CLAIM_OPEN, CLAIM_USED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    /// Kills an unspent claim, because the nonce was given back. A spent claim stays spent.
    pub fn release(&self) {
        let _ = self.state.compare_exchange(
            CLAIM_OPEN,
            CLAIM_RELEASED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// The store primitive behind a claim: one atomic "insert if absent" of a nonce on a link.
///
/// This is the only thing a store implements to let its callers claim nonces; the claim itself
/// is built by [`claim_with`], which calls [`insert_new`](Self::insert_new) and nothing else, so
/// no store can hand out a claim without having inserted the nonce.
pub trait NonceClaimer {
    /// Inserts `nonce` for `link` atomically. `Ok(true)` only for the caller that inserted it,
    /// `Ok(false)` when it was already there.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] (or another store refusal) when the store cannot answer.
    fn insert_new(&self, link: &LinkId, nonce: &Nonce) -> Result<bool>;
}

/// The asynchronous form of [`NonceClaimer`], for stores that reach the database with `await`.
pub trait AsyncNonceClaimer {
    /// Inserts `nonce` for `link` atomically; same contract as [`NonceClaimer::insert_new`].
    ///
    /// # Errors
    /// A store refusal when the store cannot answer.
    fn insert_new(
        &self,
        link: &LinkId,
        nonce: &Nonce,
    ) -> impl std::future::Future<Output = Result<bool>> + Send;
}

/// Claims `nonce` on `link` in `store`: `Ok(Some(claim))` when the store inserted it now,
/// `Ok(None)` when it was already there. The only code path that builds a [`NonceClaim`] for a
/// caller outside this crate.
///
/// # Errors
/// Whatever the store's [`NonceClaimer::insert_new`] returns.
pub fn claim_with<C: NonceClaimer + ?Sized>(
    store: &C,
    link: &LinkId,
    nonce: &Nonce,
) -> Result<Option<NonceClaim>> {
    Ok(store
        .insert_new(link, nonce)?
        .then(|| NonceClaim::granted(*link, *nonce)))
}

/// The asynchronous [`claim_with`].
///
/// # Errors
/// Whatever the store's [`AsyncNonceClaimer::insert_new`] returns.
pub async fn claim_with_async<C: AsyncNonceClaimer + Sync>(
    store: &C,
    link: &LinkId,
    nonce: &Nonce,
) -> Result<Option<NonceClaim>> {
    Ok(store
        .insert_new(link, nonce)
        .await?
        .then(|| NonceClaim::granted(*link, *nonce)))
}

/// Durable storage of the nonces a link's handshake has used.
///
/// Implementations make `record` atomic: when two callers race with the same nonce for the
/// same link, exactly one sees `Ok(true)`. A store failure is an error, never `false`: a log
/// that cannot answer must not be read as "this nonce is new".
pub trait NonceLog: Send + Sync {
    /// Records `nonce` as used on `link`.
    ///
    /// Returns `Ok(false)` when the nonce was already recorded for that link.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] (or another store-specific refusal) when the log cannot answer.
    fn record(&self, link: &LinkId, nonce: &Nonce) -> Result<bool>;

    /// Records the nonce of a message whose caller already claimed it, through `claim`.
    ///
    /// Returns `Ok(true)` when `claim` is for `nonce` on `link`, is spent now for the first time,
    /// and the log really holds the nonce. Returns `Ok(false)` otherwise (a second use, another
    /// nonce or link, a released claim, a nonce the log does not hold): the message is a replay
    /// as far as this call is concerned. The default refuses with a store error: a log that
    /// cannot check a claim must not accept one.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] when the log cannot answer or does not support claims.
    fn record_claimed(&self, _link: &LinkId, _nonce: &Nonce, _claim: &NonceClaim) -> Result<bool> {
        Err(Error::StoreFailed {
            reason: "this nonce log does not accept claims",
        })
    }

    /// Gives back a nonce this caller just recorded, because the step it guarded did not
    /// happen (a failed commit): without it a genuine message that failed once would be refused
    /// as a replay when resent. Only `LinkService::receive_revoke` calls it, for a nonce it
    /// recorded itself in the same call.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] when the log cannot answer.
    fn release(&self, link: &LinkId, nonce: &Nonce) -> Result<()>;
}

/// The in-memory [`NonceLog`], for tests and single-process hosts.
///
/// It is deliberately not a database: a host that must survive a restart implements the trait
/// over its own storage. The handshake tests use two instances to show what a lost log means.
#[derive(Default)]
pub struct MemoryNonceLog {
    seen: Mutex<BTreeSet<(LinkId, Nonce)>>,
}

impl MemoryNonceLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `nonce` is already recorded for `link`.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] when the lock is poisoned.
    pub fn contains(&self, link: &LinkId, nonce: &Nonce) -> Result<bool> {
        self.seen
            .lock()
            .map(|seen| seen.contains(&(*link, *nonce)))
            .map_err(|_| Error::StoreFailed {
                reason: "the nonce log lock is poisoned",
            })
    }
}

impl MemoryNonceLog {
    /// Claims `nonce` on `link` for the caller: `Ok(Some(claim))` when it was new, `Ok(None)` when
    /// it was already recorded. The in-memory counterpart of a store's claim step.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] when the lock is poisoned.
    pub fn claim(&self, link: &LinkId, nonce: &Nonce) -> Result<Option<NonceClaim>> {
        claim_with(self, link, nonce)
    }
}

impl NonceClaimer for MemoryNonceLog {
    fn insert_new(&self, link: &LinkId, nonce: &Nonce) -> Result<bool> {
        self.record(link, nonce)
    }
}

impl NonceLog for MemoryNonceLog {
    fn record_claimed(&self, link: &LinkId, nonce: &Nonce, claim: &NonceClaim) -> Result<bool> {
        // Check that the log holds the nonce before the claim is spent, so a claim for a nonce
        // this log never saw is refused and stays unspent.
        if !self.contains(link, nonce)? {
            return Ok(false);
        }
        Ok(claim.spend(link, nonce))
    }

    fn release(&self, link: &LinkId, nonce: &Nonce) -> Result<()> {
        self.seen
            .lock()
            .map(|mut seen| {
                seen.remove(&(*link, *nonce));
            })
            .map_err(|_| Error::StoreFailed {
                reason: "the nonce log lock is poisoned",
            })
    }

    fn record(&self, link: &LinkId, nonce: &Nonce) -> Result<bool> {
        let mut seen = self.seen.lock().map_err(|_| Error::StoreFailed {
            reason: "the nonce log lock is poisoned",
        })?;
        Ok(seen.insert((*link, *nonce)))
    }
}
