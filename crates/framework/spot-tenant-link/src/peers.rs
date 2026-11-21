//! Pinned peer identity keys and how a first-use key may be trusted.
//!
//! The key in a handshake message proves only that its sender holds the matching private key;
//! it does not prove that the key belongs to the party the message names. Taking the first
//! key a party sends would let anyone who can reach the other tenant stand in for that party.
//! A side therefore looks the peer's key up in a [`PeerKeyDirectory`] it already trusts — the
//! control-plane registry or the tenant's own key table (WP-CTL-09.T2) — and refuses a key that
//! differs.
//!
//! When the directory holds no key yet, the handshake refuses the message unless the caller
//! passes a [`Trust::FirstUseConfirmed`] carrying the fingerprint an administrator typed or
//! scanned out of band. Confirming a fingerprint is an explicit act on every first use; it is
//! never implied by the message arriving.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::keys::PeerKey;
use crate::model::Party;

/// The identity keys this side already trusts, by party.
///
/// The answer comes from a source this side controls, not from the message being checked. `None`
/// therefore means "no key is pinned for this party", and the handshake treats that as a refusal
/// unless the caller confirmed the first-use fingerprint.
pub trait PeerKeyDirectory: Send + Sync {
    /// The pinned identity key for `party`, if one is recorded.
    fn pinned(&self, party: &Party) -> Option<PeerKey>;
}

/// How a peer's identity key may be accepted when the directory has no pin for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Trust {
    /// Only a key the directory already pins is accepted; a first-use key is refused.
    #[default]
    PinnedOnly,
    /// An administrator confirmed this fingerprint out of band; it must equal the fingerprint
    /// of the key in the message, and only for a party with no pinned key.
    FirstUseConfirmed {
        /// The fingerprint the administrator typed or scanned (lower-case hex SHA-256).
        fingerprint: String,
    },
}

impl Trust {
    /// A first-use confirmation of `key`'s fingerprint, as a host does after the administrator
    /// reads it out to the peer and back.
    #[must_use]
    pub fn first_use(key: PeerKey) -> Self {
        Self::FirstUseConfirmed {
            fingerprint: key.fingerprint(),
        }
    }

    /// Whether `key` matches this trust setting. Used only when nothing is pinned.
    #[must_use]
    pub fn confirms(&self, key: PeerKey) -> bool {
        match self {
            Self::PinnedOnly => false,
            Self::FirstUseConfirmed { fingerprint } => {
                fingerprint.trim().eq_ignore_ascii_case(&key.fingerprint())
            }
        }
    }
}

/// The in-memory [`PeerKeyDirectory`], for tests and single-process hosts.
#[derive(Default)]
pub struct MemoryPeerKeyDirectory {
    keys: Mutex<BTreeMap<Party, PeerKey>>,
}

impl MemoryPeerKeyDirectory {
    /// An empty directory: every party is on first use.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pins `key` for `party`.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] when the lock is poisoned.
    pub fn pin(&self, party: Party, key: PeerKey) -> Result<()> {
        self.keys
            .lock()
            .map(|mut keys| {
                keys.insert(party, key);
            })
            .map_err(|_| Error::StoreFailed {
                reason: "the peer key directory lock is poisoned",
            })
    }

    /// A directory with one party already pinned.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] when the lock is poisoned.
    pub fn with(party: Party, key: PeerKey) -> Result<Self> {
        let directory = Self::new();
        directory.pin(party, key)?;
        Ok(directory)
    }
}

impl PeerKeyDirectory for MemoryPeerKeyDirectory {
    fn pinned(&self, party: &Party) -> Option<PeerKey> {
        // A poisoned lock still holds the map; recovering it keeps the directory's answer rather
        // than silently telling the handshake that nothing is pinned.
        let keys = self
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        keys.get(party).copied()
    }
}

/// Checks the first identity key a party sends against the pins this side already holds.
///
/// A pinned party must present exactly its pinned key. A party with no pin is accepted only when
/// `trust` confirms this key's fingerprint; otherwise the key is refused. The handshake applies
/// this to the first key each party sends, and a store that screens inbound messages before it
/// claims their nonces applies the same rule.
///
/// # Errors
/// [`Error::KeyMismatch`] for a key that differs from the pin or from the confirmed
/// fingerprint, [`Error::KeyUnpinned`] when nothing is pinned and nothing was confirmed.
pub fn check_peer_key(
    keys: &dyn PeerKeyDirectory,
    trust: &Trust,
    party: &Party,
    key: PeerKey,
) -> Result<()> {
    if let Some(pinned) = keys.pinned(party) {
        if pinned != key {
            return Err(Error::KeyMismatch {
                party: party.canonical(),
                expected: pinned.fingerprint(),
                found: key.fingerprint(),
            });
        }
        return Ok(());
    }
    if trust.confirms(key) {
        return Ok(());
    }
    if let Trust::FirstUseConfirmed { fingerprint } = trust {
        return Err(Error::KeyMismatch {
            party: party.canonical(),
            expected: fingerprint.trim().to_owned(),
            found: key.fingerprint(),
        });
    }
    Err(Error::KeyUnpinned {
        party: party.canonical(),
    })
}
