//! Sealed persistence: the epoch and lease state is ciphertext on the node's disk.
//!
//! [`SealedPersistence`] implements the core crate's
//! [`Persistence`](spot_offline_identity::Persistence) over a small two-slot store. Before a
//! [`Persisted`] is written it is sealed with the node's own key under associated data that
//! names this node, so the stored bytes are useless on another node and cannot be altered:
//!
//! - **sealed for another node**: the associated data names the node, so a state copied to
//!   another node's store does not open ([`AdapterError::StateTampered`]);
//! - **tampered**: the AEAD authenticates every byte, so a changed byte does not open;
//! - **rolled back**: the highest epoch ever written lives in a second sealed slot (the
//!   *epoch fence*). A stored state below the fence is refused
//!   ([`AdapterError::StateRollback`]), and [`save`](Persistence::save) never writes a lower
//!   epoch than the node has seen. The fence only ever rises.
//!
//! A refused load is an error from [`Persistence::load`], so
//! [`SnapshotStore::open`](spot_offline_identity::SnapshotStore::open) reports it through
//! `recovered_from_damage` and starts with **no snapshot**: every authorisation denies until
//! the hub sends a fresh one.
//!
//! # The order of a save
//!
//! The state is written first and the fence second. If the fence write fails the save fails
//! (the caller does not commit) but the newer state is already durable, and a later load still
//! accepts it because a state at or above the fence is never a rollback. The other order would
//! lose a state that was written successfully.
//!
//! # What this does not do
//!
//! A party who can write *both* slots can replay an older state **and** its older fence while
//! the node is off; no local format can detect that without a monotonic anchor outside the disk
//! (a TPM counter or the hub). The node then runs on the old image, and its authority (a revoked
//! user stays active) is bounded by its lease and nothing else, until it reaches the hub. The
//! hub detects the replay at the next contact, because the node's signed request reports the
//! epoch, snapshot hash and hub nonce it holds (see `spot-offline-identity`). Within the node,
//! and against a swap of the state alone, the fence is what makes the epoch monotonic.
//!
//! The fence slot also holds the newest hub nonce seen (`nonce_floor`). A lease image older
//! than that restores no time on another boot, so putting back the state slot alone, or an
//! image from before a later hub contact, does not hand back an old lease. The slot is 16 bytes
//! (epoch, then nonce); a slot of 8 bytes written by an earlier version holds the epoch only
//! and reads as nonce zero.
//!
//! # Keys
//!
//! The key is a [`LockedKey`] the host loads from `spot-secrets` (a key provider) or
//! `spot-protected-keys` (the node's key material). It never leaves locked memory, is never
//! printed, and is not part of any state, error or log.

use std::sync::atomic::{AtomicU64, Ordering};

use spot_offline_identity::{NodeId, PersistError, Persisted, Persistence};
use spot_protected_keys::{FieldAd, FieldCipher, KeyEpoch, LockedKey, TenantId as KeyTenantId};

use crate::error::AdapterError;

/// The state format. It is part of the associated data, so a later format cannot be opened as
/// this one.
const DOCTYPE: &str = concat!("offline_identity", ".state.v1");
/// The slot the sealed state sits in.
const FIELD_STATE: &str = "state";
/// The slot the sealed epoch fence sits in.
const FIELD_FENCE: &str = "epoch_fence";

/// Which of the two slots a [`SealedStateStore`] read or write is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Slot {
    /// The sealed [`Persisted`] state.
    State,
    /// The sealed highest epoch ever written.
    Fence,
}

/// Where the node keeps the sealed state and its epoch fence. A write must be durable when it
/// returns; a state and its fence must survive a crash together (see the module note).
pub trait SealedStateStore {
    /// The bytes in `slot`, or `None` when it was never written.
    ///
    /// # Errors
    /// [`AdapterError::StateRead`] when the store cannot read.
    fn read(&self, slot: Slot) -> Result<Option<Vec<u8>>, AdapterError>;

    /// Replaces the bytes of `slot`.
    ///
    /// # Errors
    /// [`AdapterError::StateWrite`] when the bytes are not durably stored.
    fn write(&mut self, slot: Slot, bytes: &[u8]) -> Result<(), AdapterError>;
}

/// An in-memory [`SealedStateStore`], for tests and single-process use.
#[derive(Clone, Debug, Default)]
pub struct MemoryStateStore {
    /// The sealed state, for tests that inject or read raw bytes.
    pub state: Option<Vec<u8>>,
    /// The sealed epoch fence, for tests that inject or read raw bytes.
    pub fence: Option<Vec<u8>>,
    /// When set, every write fails.
    pub fail_writes: bool,
}

impl SealedStateStore for MemoryStateStore {
    fn read(&self, slot: Slot) -> Result<Option<Vec<u8>>, AdapterError> {
        Ok(match slot {
            Slot::State => self.state.clone(),
            Slot::Fence => self.fence.clone(),
        })
    }

    fn write(&mut self, slot: Slot, bytes: &[u8]) -> Result<(), AdapterError> {
        if self.fail_writes {
            return Err(AdapterError::StateWrite);
        }
        match slot {
            Slot::State => self.state = Some(bytes.to_vec()),
            Slot::Fence => self.fence = Some(bytes.to_vec()),
        }
        Ok(())
    }
}

/// A [`Persistence`] that seals the epoch and lease state, bound to one node.
pub struct SealedPersistence<S: SealedStateStore> {
    store: S,
    key: LockedKey,
    tenant: KeyTenantId,
    node: String,
    /// The highest epoch this node has seen, in this process or on disk. Only ever rises.
    fence: AtomicU64,
    /// The newest hub nonce this node has seen, in this process or on disk. Only ever rises.
    nonce_fence: AtomicU64,
}

impl<S: SealedStateStore> SealedPersistence<S> {
    /// The sealed persistence of `node` of `tenant`, keyed by `key`, over `store`.
    ///
    /// Nothing is read here: [`load`](Persistence::load) reads, so a damaged store is reported
    /// by the store that opens it.
    pub fn new(store: S, key: LockedKey, tenant: KeyTenantId, node: &NodeId) -> Self {
        Self {
            store,
            key,
            tenant,
            node: node.as_str().to_owned(),
            fence: AtomicU64::new(0),
            nonce_fence: AtomicU64::new(0),
        }
    }

    /// The highest epoch this node has seen (zero before the first state).
    #[must_use]
    pub fn fence(&self) -> u64 {
        self.fence.load(Ordering::SeqCst)
    }

    /// The newest hub nonce this node has seen (zero before the first).
    #[must_use]
    pub fn nonce_fence(&self) -> u64 {
        self.nonce_fence.load(Ordering::SeqCst)
    }

    /// The store (tests and the owner read it).
    #[must_use]
    pub fn into_store(self) -> S {
        self.store
    }

    /// [`Persistence::load`], with the reason when it fails.
    ///
    /// # Errors
    /// [`AdapterError::StateRead`] when the store cannot read or the fence has no state,
    /// [`AdapterError::StateTampered`] when a slot does not authenticate (altered, or sealed
    /// for another node), [`AdapterError::StateMalformed`] when a slot is not this format, and
    /// [`AdapterError::StateRollback`] when the state is older than the fence.
    pub fn load_state(&self) -> Result<Option<Persisted>, AdapterError> {
        let (stored_fence, stored_nonce) = match self.store.read(Slot::Fence)? {
            Some(raw) => self.open_fence(&raw)?,
            None => (0, 0),
        };
        self.nonce_fence.fetch_max(stored_nonce, Ordering::SeqCst);
        // The fence authenticated: the node has seen this epoch, whatever else is wrong with
        // the state. Raise it before anything below can refuse the load, so a refused load
        // never leaves the node willing to write (or re-accept) an older epoch.
        self.raise_fence(stored_fence);
        let Some(raw) = self.store.read(Slot::State)? else {
            // A fence with no state means the state was removed; with neither slot written,
            // the node simply has no state yet.
            return if stored_fence == 0 && stored_nonce == 0 {
                Ok(None)
            } else {
                Err(AdapterError::StateRead)
            };
        };
        let plain = self.open(FIELD_STATE, &raw)?;
        let state: Persisted =
            serde_json::from_slice(&plain).map_err(|_| AdapterError::StateMalformed)?;
        // The state authenticated: its epoch was seen too.
        self.raise_fence(state.highest_epoch);
        self.nonce_fence
            .fetch_max(state.lease.nonce, Ordering::SeqCst);
        if state.highest_epoch < stored_fence {
            return Err(AdapterError::StateRollback {
                stored: state.highest_epoch,
                seen: stored_fence,
            });
        }
        Ok(Some(state))
    }

    /// [`Persistence::save`], with the reason when it fails.
    ///
    /// # Errors
    /// [`AdapterError::StateRollback`] when `state`'s epoch is below the fence,
    /// [`AdapterError::StateWrite`] when the store refuses a slot, and
    /// [`AdapterError::StateMalformed`] when the state cannot be encoded.
    pub fn save_state(&mut self, state: &Persisted) -> Result<(), AdapterError> {
        let fence = self.fence();
        if state.highest_epoch < fence {
            return Err(AdapterError::StateRollback {
                stored: state.highest_epoch,
                seen: fence,
            });
        }
        let plain = serde_json::to_vec(state).map_err(|_| AdapterError::StateMalformed)?;
        let sealed = self.seal(FIELD_STATE, &plain)?;
        self.store.write(Slot::State, &sealed)?;
        let nonce_fence = self.nonce_fence();
        if state.highest_epoch > fence || state.lease.nonce > nonce_fence {
            let epoch = state.highest_epoch.max(fence);
            let nonce = state.lease.nonce.max(nonce_fence);
            let mut plain = [0u8; 16];
            plain[..8].copy_from_slice(&epoch.to_be_bytes());
            plain[8..].copy_from_slice(&nonce.to_be_bytes());
            let sealed_fence = self.seal(FIELD_FENCE, &plain)?;
            self.store.write(Slot::Fence, &sealed_fence)?;
            self.fence.store(epoch, Ordering::SeqCst);
            self.nonce_fence.store(nonce, Ordering::SeqCst);
        }
        Ok(())
    }

    fn raise_fence(&self, epoch: u64) {
        self.fence.fetch_max(epoch, Ordering::SeqCst);
    }

    fn ad(&self, field: &'static str) -> FieldAd {
        FieldAd::new(
            self.tenant.clone(),
            DOCTYPE,
            field,
            self.node.clone(),
            KeyEpoch::FIRST,
        )
    }

    fn seal(&self, field: &'static str, plain: &[u8]) -> Result<Vec<u8>, AdapterError> {
        FieldCipher::encrypt(&self.key, &self.ad(field), plain)
            .map_err(|_| AdapterError::StateWrite)
    }

    fn open(&self, field: &'static str, sealed: &[u8]) -> Result<Vec<u8>, AdapterError> {
        FieldCipher::decrypt(&self.key, &self.ad(field), sealed)
            .map(|secret| secret.expose().to_vec())
            .map_err(|_| AdapterError::StateTampered)
    }

    /// The fence slot: 16 bytes (epoch, nonce), or 8 bytes (epoch only) from an earlier version.
    fn open_fence(&self, sealed: &[u8]) -> Result<(u64, u64), AdapterError> {
        let plain = self.open(FIELD_FENCE, sealed)?;
        let word = |at: usize| -> Result<u64, AdapterError> {
            let bytes: [u8; 8] = plain
                .get(at..at + 8)
                .and_then(|b| b.try_into().ok())
                .ok_or(AdapterError::StateMalformed)?;
            Ok(u64::from_be_bytes(bytes))
        };
        match plain.len() {
            8 => Ok((word(0)?, 0)),
            16 => Ok((word(0)?, word(8)?)),
            _ => Err(AdapterError::StateMalformed),
        }
    }
}

impl<S: SealedStateStore> Persistence for SealedPersistence<S> {
    fn load(&self) -> Result<Option<Persisted>, PersistError> {
        self.load_state().map_err(|_| PersistError)
    }

    fn save(&mut self, state: &Persisted) -> Result<(), PersistError> {
        self.save_state(state).map_err(|_| PersistError)
    }

    fn nonce_floor(&self) -> u64 {
        self.nonce_fence()
    }
}
