//! `spot-protected-keys`: the key hierarchy behind field-level protection on branch and
//! offline nodes (WP-SEC-10.T1, ADR-070 rule 12.13 e).
//!
//! A protection group (for example `bank_details`) is encrypted with **one data key per
//! epoch**. The hub holds the plaintext data key in memory and stores only its **wrapped**
//! form; a node never receives the bare key, only a copy wrapped under its own per-node
//! wrapping key, released under a **lease** (item d). Revoking a node stops key release and
//! rotates the data key of every group the node held, so a revoked node cannot open data
//! written after rotation. The tenant **root key** sits above all of it and is held by a
//! [`RootKeyProvider`] — a KMS, an HSM or, in tests, [`MemoryRootKeyProvider`].
//!
//! # Shape
//!
//! - [`locked`]: [`LockedKey`], key material that is zeroised on drop, cannot be cloned or
//!   printed, and is pinned through the [`MemoryLock`] seam (`mlock`, no core dumps).
//! - [`ids`]: [`TenantId`], [`GroupName`], [`NodeId`] and the monotonic [`KeyEpoch`].
//! - [`root`]: [`TenantRootKey`] and the [`RootKeyProvider`] trait.
//! - [`keys`]: [`GroupDataKey`] and [`NodeWrapKey`].
//! - [`hub`]: [`Hub`], which creates and rotates group keys, registers nodes and issues a
//!   [`KeyRelease`].
//! - [`node`]: [`NodeKeyStore`], the node's lease-bound memory of released keys.
//! - [`field`]: [`FieldCipher`] (randomised AEAD with associated-data binding) and the
//!   separately named [`BlindIndexKey`].
//! - [`reencrypt`]: the resumable [`ReencryptPlan`] a rotation produces.
//! - [`store`]: [`KeyStoreBackup`], which holds only wrapped keys, and the [`KeyStore`]
//!   persistence port (wrapped keys per group and epoch, node keys, leases, revocation, an
//!   append-only history) with the in-memory [`MemoryKeyStore`] (WP-SEC-10.T4).
//!
//! # Rules (the behaviour this crate is, in our own words)
//!
//! - A node receives a group key **only** wrapped to its node key. The hub wraps with the
//!   same AEAD family as `spot-secrets`; nothing here invents cryptography.
//! - Release checks that the node is active, that the permission snapshot gives it a tier for
//!   the group ([`PermissionTier`]) and that it is not revoked. The lease ends at
//!   `lease_until`; the node discards the key when its **monotonic** clock passes it.
//! - Revocation rotates every group the node held and stops release to it. Old epochs stay
//!   readable by authorised holders for old data; a re-encryption job is a separate plan.
//! - Field encryption is randomised and bound to `(tenant, doctype, field, document, epoch)`,
//!   so a ciphertext moved to another field fails to open. The only deterministic function is
//!   the blind index, and it uses its own key.
//! - No key material ever reaches `Debug`, `Display`, an error, serde, a log or a backup.
//!
//! Time is always an argument ([`MonoTime`]); this crate never reads a clock.

mod bind;
mod crypto;
pub mod error;
pub mod field;
pub mod hub;
pub mod ids;
pub mod keys;
pub mod locked;
mod memory_store;
pub mod node;
pub mod reencrypt;
pub mod release;
pub mod root;
pub mod store;
pub mod tier;
pub mod time;

pub use error::ProtectedKeyError;
pub use field::{BlindIndex, BlindIndexKey, FieldAd, FieldCipher};
pub use hub::{Hub, NodeStatus, RevocationReport, RotatedGroup};
pub use ids::{GroupName, KeyEpoch, NodeId, TenantId};
pub use keys::{GroupDataKey, NodeWrapKey};
pub use locked::{LockedKey, MemoryLock, NoLock};
pub use memory_store::MemoryKeyStore;
pub use node::NodeKeyStore;
pub use reencrypt::{ReencryptBatch, ReencryptPlan};
pub use release::KeyRelease;
pub use root::{MemoryRootKeyProvider, RootKeyProvider, TenantRootKey};
pub use store::{
    Audit, KeyEvent, KeyEventKind, KeyStore, KeyStoreBackup, KeyStoreError, RefusalReason,
    ReleaseRecord, RootRef, Rotation, StoredNode, WrappedGroupKey, WrappedNodeKey,
};
pub use tier::{PermissionTier, Tier, TierName, TierTable};
pub use time::{Duration, MonoTime};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-protected-keys";

/// The length of a key in bytes (256-bit), shared with the vetted AEAD layer.
pub const KEY_LEN: usize = locked::KEY_LEN;
