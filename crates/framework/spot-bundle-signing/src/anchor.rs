//! The store's trust anchor: root keys with validity windows, so the root can rotate.
//!
//! A root signs publisher certificates and revocation lists. Retiring a root is a window that
//! closes; the new root's window opens first, so both trust the same content for a while.

use crate::keys::{KeyId, PublicKey};

/// One trusted root key and its window. `valid_to` of `None` means no end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedRoot {
    key_id: KeyId,
    public: PublicKey,
    /// First instant (UTC seconds) the root is trusted for.
    pub valid_from: i64,
    /// First instant the root is no longer trusted; `None` for no end.
    pub valid_to: Option<i64>,
}

impl TrustedRoot {
    /// A trusted root for `public`, valid over the given window.
    #[must_use]
    pub fn new(public: PublicKey, valid_from: i64, valid_to: Option<i64>) -> Self {
        Self {
            key_id: public.key_id(),
            public,
            valid_from,
            valid_to,
        }
    }

    /// The root's key id.
    #[must_use]
    pub fn key_id(&self) -> &KeyId {
        &self.key_id
    }

    /// The root's public key.
    #[must_use]
    pub fn public(&self) -> PublicKey {
        self.public
    }

    /// Whether the root is trusted at `now`.
    #[must_use]
    pub fn valid_at(&self, now: i64) -> bool {
        self.valid_from <= now && self.valid_to.is_none_or(|end| now < end)
    }
}

/// The roots this store trusts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustAnchor {
    roots: Vec<TrustedRoot>,
}

impl TrustAnchor {
    /// An anchor over `roots`.
    #[must_use]
    pub fn new(roots: Vec<TrustedRoot>) -> Self {
        Self { roots }
    }

    /// The root with this key id, if the store trusts it at all.
    #[must_use]
    pub fn root(&self, id: &KeyId) -> Option<&TrustedRoot> {
        self.roots.iter().find(|root| root.key_id() == id)
    }
}
