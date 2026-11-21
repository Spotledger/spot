//! The durable handshake ports: [`PgNonceLog`] and [`PgPeerKeyDirectory`] (WP-CTL-09.T6).
//!
//! `MemoryNonceLog` forgets every used nonce when the process restarts, so a captured handshake
//! message replays; `MemoryPeerKeyDirectory` forgets every pinned key, so each peer must be
//! confirmed again. A host builds both from the one store with
//! [`PgLinkStore::handshake_ports`](crate::PgLinkStore::handshake_ports) and hands them to
//! `HandshakePolicy`. A production host must not wire the `Memory*` types.
//!
//! # Nonces
//!
//! `record` inserts into `spot_tenant_link_nonce` with `ON CONFLICT DO NOTHING`: `Ok(true)` for
//! the one caller whose insert took effect, `Ok(false)` for a nonce seen before. A database
//! failure is `Err`, never `false`, so a log that cannot answer refuses the message. The row's
//! `seen_at` is the **database** clock in milliseconds (the trait carries no instant and this
//! crate reads no clock of its own), the same unit [`PgLinkStore::prune_nonces`] compares
//! against. The adapter never prunes by itself: the job engine calls `prune_nonces`.
//!
//! # Pins
//!
//! A pin is the public key recorded for a party in `spot_tenant_link_peer_key`; the table is
//! written with the link row, so a pin exists once a handshake has exchanged keys on any link of
//! this tenant. `pinned` reads that table under the tenant's own schema.
//!
//! The trait answers `Option<PeerKey>`, so a failed read cannot be told apart from "no pin"
//! by the handshake. The directory therefore returns `None` **and records the failure**
//! ([`PgPeerKeyDirectory::last_error`]); the handshake refuses a `None` unless the caller passed
//! `Trust::FirstUseConfirmed`. A host must not offer a first-use confirmation while
//! `last_error()` is set, and should read pins it acts on through
//! [`PgPeerKeyDirectory::pinned_checked`], which returns the failure as an `Err`. Two different
//! keys recorded for one party (on different links) are also a failure, never a choice of one.

use std::sync::{Arc, Mutex};

use spot_tenant_link::{
    Error, LinkId, Nonce, NonceClaim, NonceLog, Party, PeerKey, PeerKeyDirectory, Result,
};

use crate::error::{query_failed, store_failed};
use crate::store::PgLinkStore;

/// The database clock in milliseconds since the epoch.
const NOW_MS: &str = "floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint";

/// The durable [`NonceLog`] of one tenant's link store.
pub struct PgNonceLog {
    store: PgLinkStore,
}

impl std::fmt::Debug for PgNonceLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgNonceLog").finish_non_exhaustive()
    }
}

impl PgNonceLog {
    pub(crate) fn new(store: PgLinkStore) -> Self {
        Self { store }
    }

    async fn record_inner(&self, link: &LinkId, nonce: &Nonce) -> Result<bool> {
        let mut conn = self.store.conn().await?;
        let tx = self.store.scoped(&mut conn, false).await?;
        let inserted = tx
            .execute(
                &format!(
                    "INSERT INTO spot_tenant_link_nonce (link, nonce, seen_at) \
                     VALUES ($1, $2, {NOW_MS}) ON CONFLICT (link, nonce) DO NOTHING"
                ),
                &[&link.as_uuid(), &nonce.as_bytes().as_slice()],
            )
            .await
            .map_err(query_failed)?;
        tx.commit().await.map_err(query_failed)?;
        Ok(inserted == 1)
    }
}

impl NonceLog for PgNonceLog {
    fn record_claimed(&self, link: &LinkId, nonce: &Nonce, claim: &NonceClaim) -> Result<bool> {
        // The row must be there (a claim for a nonce this tenant never claimed is refused and
        // stays unspent); the claim then wins exactly once, for its own link and nonce.
        if !claim.is_for(link, nonce) || !self.store.run(self.store.holds_nonce(link, nonce))? {
            return Ok(false);
        }
        Ok(claim.spend(link, nonce))
    }

    fn record(&self, link: &LinkId, nonce: &Nonce) -> Result<bool> {
        self.store.run(self.record_inner(link, nonce))
    }

    fn release(&self, link: &LinkId, nonce: &Nonce) -> Result<()> {
        self.store.run(self.store.release_nonce(link, nonce))
    }
}

/// The durable [`PeerKeyDirectory`] of one tenant's link store.
pub struct PgPeerKeyDirectory {
    store: PgLinkStore,
    failure: Arc<Mutex<Option<&'static str>>>,
}

impl std::fmt::Debug for PgPeerKeyDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgPeerKeyDirectory").finish_non_exhaustive()
    }
}

impl PgPeerKeyDirectory {
    pub(crate) fn new(store: PgLinkStore) -> Self {
        Self {
            store,
            failure: Arc::new(Mutex::new(None)),
        }
    }

    /// The pinned key for `party`, with a failed read reported as an error.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] when the read fails, a stored key is malformed, or the party has
    /// two different keys recorded.
    pub fn pinned_checked(&self, party: &Party) -> Result<Option<PeerKey>> {
        self.store.run(self.read(party))
    }

    /// The reason of the most recent failed read, if any read has failed since the last
    /// [`clear_error`](Self::clear_error). A later successful read does not clear it: a failure
    /// that looked like "no pin" must stay visible to the host.
    #[must_use]
    pub fn last_error(&self) -> Option<&'static str> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .copied()
    }

    /// Forgets the recorded failure, after the host has dealt with it.
    pub fn clear_error(&self) {
        *self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    async fn read(&self, party: &Party) -> Result<Option<PeerKey>> {
        let mut conn = self.store.conn().await?;
        let tx = self.store.scoped(&mut conn, true).await?;
        let rows = tx
            .query(
                "SELECT DISTINCT public_key FROM spot_tenant_link_peer_key WHERE party = $1",
                &[&party.canonical()],
            )
            .await
            .map_err(query_failed)?;
        match rows.as_slice() {
            [] => Ok(None),
            [row] => {
                let bytes: Vec<u8> = row.get(0);
                let bytes: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| store_failed("a stored peer key is malformed"))?;
                PeerKey::from_bytes(bytes)
                    .map(Some)
                    .map_err(|_| store_failed("a stored peer key is malformed"))
            }
            _ => Err(store_failed("a party has more than one pinned key")),
        }
    }
}

impl PeerKeyDirectory for PgPeerKeyDirectory {
    fn pinned(&self, party: &Party) -> Option<PeerKey> {
        match self.pinned_checked(party) {
            Ok(key) => key,
            Err(Error::StoreFailed { reason }) => {
                *self
                    .failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
                None
            }
            Err(_) => {
                *self
                    .failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some("the peer key directory read failed");
                None
            }
        }
    }
}
