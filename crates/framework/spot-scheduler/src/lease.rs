//! The lease that decides who may tick, with fencing tokens.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

use crate::error::{Error, Result};

/// A fencing token: one more than every token ever issued for the lease's name.
///
/// Tokens exist so a delayed writer from an *old* holder cannot be confused with the *current*
/// one: storage-backed writers compare the token they hold against the store's
/// ([`FencedWrite::check`]); a stale one is refused with `scheduler.lease_lost`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct FencingToken(pub u64);

/// Which lease record a holder names in `renew`/`release` — the record's issue counter, so two
/// holders that both think they own `scheduler:<site>` (one from before a crash) release or
/// renew *their own* record, never the other's.
pub type LeaseId = u64;

/// One live lease: who holds it, until when, and the fencing token it writes under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Lease {
    /// The lease's name, `scheduler:<site>` for the schedule leader.
    pub name: String,
    /// Who holds it (a node id in the simulation, a hostname/instance in production).
    pub holder: String,
    /// The fencing token this acquisition got.
    pub token: FencingToken,
    /// When the lease stops covering its holder, in the store's clock.
    pub expires_at: jiff::Timestamp,
    /// The record this handle points at, for `renew`/`release` by the right holder.
    pub id: LeaseId,
}

/// The lease table every scheduler node shares. Object-safe, so a database-backed store
/// (WP-PLT-18's coordinator) can implement the same trait.
pub trait LeaseStore: Send + Sync {
    /// Acquires the named lease or fails with `scheduler.lease_lost` when a live lease for the
    /// name exists. The token increases strictly on every *new* acquisition of the name — the
    /// first is 1, the next after release or expiry is 2, and so on for the life of the store.
    /// `now` comes from the caller's clock; the store never reads a clock itself.
    fn acquire(
        &self,
        name: &str,
        holder: &str,
        ttl: Duration,
        now: jiff::Timestamp,
    ) -> Result<Lease>;

    /// Extends the lease by `ttl` from `now`, when `lease` is still the current record; fails
    /// with `scheduler.lease_lost` from a stale holder (expired, released or superseded).
    fn renew(&self, lease: &Lease, ttl: Duration, now: jiff::Timestamp) -> Result<Lease>;

    /// Gives the lease up, when `lease` is still the current record; a stale record fails with
    /// `scheduler.lease_lost` and leaves the current holder alone.
    fn release(&self, lease: &Lease) -> Result<()>;

    /// The live lease for `name`, if any.
    fn current(&self, name: &str) -> Option<Lease>;
}

/// One name's state in an [`InMemoryLeaseStore`].
#[derive(Debug, Clone)]
struct LeaseState {
    lease: Option<Lease>,
    /// The last token ever issued for the name; the next acquisition's token is one more.
    last_token: u64,
    /// The next record id; never reused, so stale handles always miss.
    next_id: LeaseId,
}

/// A [`LeaseStore`] in one mutex, for tests and single-node hosts.
///
/// The clock is injectable: callers pass `now` on every call, so a simulation drives real
/// expiry without sleeping. Expiry is lazy — a live lease whose `expires_at` has passed stops
/// covering its holder at the next call with a later `now`.
#[derive(Debug)]
pub struct InMemoryLeaseStore {
    states: Mutex<HashMap<String, LeaseState>>,
}

impl InMemoryLeaseStore {
    /// An empty store. Tokens start at 0 and the first acquisition of every name gets 1.
    pub fn new() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
        }
    }

    /// Drops every expired lease under `now`; the expired holder's next acquire bumps the
    /// token. Called inside the lock by every command, so `current` never returns an expired
    /// lease.
    fn sweep(&self, states: &mut HashMap<String, LeaseState>, now: jiff::Timestamp) {
        for state in states.values_mut() {
            if state.lease.as_ref().is_some_and(|l| l.expires_at <= now) {
                state.lease = None;
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, LeaseState>> {
        // The map holds plain lease state; a panic elsewhere cannot leave it inconsistent.
        self.states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for InMemoryLeaseStore {
    fn default() -> Self {
        Self::new()
    }
}

impl LeaseStore for InMemoryLeaseStore {
    fn acquire(
        &self,
        name: &str,
        holder: &str,
        ttl: Duration,
        now: jiff::Timestamp,
    ) -> Result<Lease> {
        let mut states = self.lock();
        self.sweep(&mut states, now);
        let state = states.entry(name.to_owned()).or_insert_with(|| LeaseState {
            lease: None,
            last_token: 0,
            next_id: 1,
        });
        if let Some(existing) = &state.lease {
            return Err(Error::lease_lost(
                name,
                holder,
                existing.token.0,
                "another holder still owns the lease",
            ));
        }
        // Strictly increasing per name: one more than every token this name has ever had.
        state.last_token += 1;
        state.next_id += 1;
        let lease = Lease {
            name: name.to_owned(),
            holder: holder.to_owned(),
            token: FencingToken(state.last_token),
            expires_at: now
                + jiff::Span::new().seconds(ttl.as_secs().try_into().unwrap_or(i64::MAX)),
            id: state.next_id,
        };
        state.lease = Some(lease.clone());
        Ok(lease)
    }

    fn renew(&self, lease: &Lease, ttl: Duration, now: jiff::Timestamp) -> Result<Lease> {
        let mut states = self.lock();
        self.sweep(&mut states, now);
        let Some(state) = states.get_mut(&lease.name) else {
            return Err(Error::lease_lost(
                &lease.name,
                &lease.holder,
                lease.token.0,
                "the lease has no record here",
            ));
        };
        match &state.lease {
            Some(current) if current.id == lease.id && current.token == lease.token => {
                let mut renewed = current.clone();
                renewed.expires_at =
                    now + jiff::Span::new().seconds(ttl.as_secs().try_into().unwrap_or(i64::MAX));
                state.lease = Some(renewed.clone());
                Ok(renewed)
            }
            Some(current) => Err(Error::lease_lost(
                &lease.name,
                &lease.holder,
                lease.token.0,
                &format!("token {} holds it now", current.token.0),
            )),
            None => Err(Error::lease_lost(
                &lease.name,
                &lease.holder,
                lease.token.0,
                "the lease expired or was released",
            )),
        }
    }

    fn release(&self, lease: &Lease) -> Result<()> {
        let mut states = self.lock();
        let Some(state) = states.get_mut(&lease.name) else {
            return Err(Error::lease_lost(
                &lease.name,
                &lease.holder,
                lease.token.0,
                "the lease has no record here",
            ));
        };
        match &state.lease {
            Some(current) if current.id == lease.id && current.token == lease.token => {
                state.lease = None;
                Ok(())
            }
            Some(current) => Err(Error::lease_lost(
                &lease.name,
                &lease.holder,
                lease.token.0,
                &format!("token {} holds it now", current.token.0),
            )),
            None => Err(Error::lease_lost(
                &lease.name,
                &lease.holder,
                lease.token.0,
                "the lease expired or was released",
            )),
        }
    }

    fn current(&self, name: &str) -> Option<Lease> {
        let states = self.lock();
        // `current` cannot take a `now` (the trait fixes the signature), so it reports the
        // record regardless of expiry; expiry is enforced by the commands above, which sweep.
        states.get(name).and_then(|state| state.lease.clone())
    }
}

/// Whether `lease` still covers its holder: the store's record for the name must be the same
/// record (same id) with the same token. The exact check shared by [`crate::FencedWrite`] and
/// the tests.
pub fn still_holds(store: &dyn LeaseStore, lease: &Lease) -> bool {
    match store.current(&lease.name) {
        Some(current) => current.id == lease.id && current.token == lease.token,
        None => false,
    }
}
