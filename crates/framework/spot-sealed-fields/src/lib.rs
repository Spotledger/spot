//! `spot-sealed-fields`: sync classes and sealed fields on branch and offline nodes
//! (WP-SEC-10.T2, ADR-070 rule 12.13 a, f and h).
//!
//! This is the second half of WP-SEC-10. The key hierarchy ([`spot_protected_keys`]) decides
//! *which key* a node holds; this crate decides **what a node may do with a protection group's
//! values** at all.
//!
//! # The four sync classes
//!
//! Every protection group, per tenant, has one class ([`SyncClass`]):
//!
//! - [`SyncClass::Everywhere`]: syncs like any field; the node enforces the same grants
//!   offline. The platform default.
//! - [`SyncClass::Encrypted`]: stored encrypted on the node under its group data key, released
//!   under a lease ([`sync::EncryptedStore`]). After the lease passes, reads fail closed with
//!   `sealed_fields.lease_expired`.
//! - [`SyncClass::Sealed`]: the node **creates** values and can never read them back. Values are
//!   sealed to a key only the hub holds ([`sealbox`], [`node`], [`hub`]).
//! - [`SyncClass::HomeOnly`]: does not sync to a branch; reads there show
//!   [`sync::BranchValue::Unavailable`].
//!
//! A group tagged `secret`, `pii` or `financial` has **no default**: reading its class without
//! an explicit choice is [`SealedFieldsError::ClassUnset`].
//!
//! # Sealed fields and the compile-time split
//!
//! [`box::seal`] is an anonymous-sender public-key box to the hub's X25519 key: the sender
//! needs no key and no identity is carried, and only the holder of the private key opens the
//! result. A node build holds [`box::HubPublicKey`] and nothing else. [`hub::HubKeys`] and the
//! one `unseal` live in the `hub` module, which `lib.rs` gates on `not(feature = "node")`:
//! built with `--features node` the module, the trait and the function
//! are not in the binary at all (see `tests/node_build.rs`).
//!
//! # Reveal is online-only
//!
//! [`reveal::RevealService`] checks the field's `reveal` right through `spot-permissions`
//! (whose `explain_field` is the support view), loads the value from the hub's own storage,
//! writes exactly one audit record **before** returning it, and refuses when the record cannot
//! be written. [`reveal::RevealService::reveal_bulk`] is for service principals only and only
//! for a group their manifest declares. Offline, [`reveal::offline_reveal`] answers
//! [`RevealError::Offline`] with a user-facing explanation key.
//!
//! # Channel exclusion
//!
//! [`channel::ChannelGuard`] is the one function every emitting channel calls: it answers
//! `Emit`, `Mask` or `Drop` from the class and the destination, so a `home_only`, `sealed` or
//! `encrypted` group cannot leak through the journal, a bootstrap snapshot, a search or
//! embedding feeder, an audit diff or a backup. [`channel::builtin_registry`] carries every
//! channel ADR-070 names, and a test fails if one has no registration; another crate appends
//! its own by implementing [`channel::GuardedChannel`].
//!
//! # Purity
//!
//! No storage, no network, no ambient reads, no floats, no `unsafe`, no `unwrap`/`expect`
//! outside tests. Time is always an argument; this crate never reads a clock.

#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

pub mod bind;
pub mod channel;
pub mod class;
pub mod error;
pub mod node;
pub mod reveal;
pub mod sealbox;
pub mod sync;

#[cfg(not(feature = "node"))]
pub mod hub;

pub use bind::{SealedAd, class_row, reveal_subject};
pub use channel::{
    ADR_CHANNELS, BootstrapSnapshot, ChannelGuard, ChannelRegistry, Destination, Emission,
    GuardedChannel, builtin_registry,
};
pub use class::{NO_DEFAULT_TAGS, SETTING_KEY, SyncClass, SyncClassTable, has_no_default};
pub use error::{
    AUDIT_UNAVAILABLE, CHANNEL_UNREGISTERED, CLASS_INVALID, CLASS_UNSET, GROUP_UNDECLARED,
    HOME_ONLY, HUB_UNAVAILABLE, LEASE_EXPIRED, MALFORMED, NODE_REVOKED, REVEAL_BULK_DENIED,
    REVEAL_DENIED, REVEAL_OFFLINE, RevealError, SEALED_UNREADABLE, SETTING_INVALID,
    SealedFieldsError, TAMPERED, UNKNOWN_GROUP,
};
#[cfg(not(feature = "node"))]
pub use hub::{HubKeys, LocalHub, LocalHubKeys};
pub use node::{NodeFieldStore, SealTarget, SealedField};
pub use reveal::{
    AuditSink, BulkRequest, HubPort, Loaded, MemoryAuditSink, NodeIdentity, OFFLINE_EXPLANATION,
    REVEAL_API, REVEAL_AUDIT_KIND, REVEAL_BULK_AUDIT_KIND, RemoteReveal, RevealService, Revealed,
    ServiceManifest, ValueSource, offline_reveal,
};
#[cfg(not(feature = "node"))]
pub use sealbox::{BoxSecretKey, open};
pub use sealbox::{
    HubPublicKey, PUBLIC_KEY_LEN, Sealed, SealedWriter, SealingKey, seal, seal_bound,
};
pub use sync::{
    BranchValue, EncryptedField, EncryptedStore, HOME_ONLY_EXPLANATION, OutgoingField,
    OutgoingValue, SyncPolicy, emission_for, filter_outgoing_batch, read_at_branch,
};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-sealed-fields";

/// Whether this build holds the hub side (`unseal`) or is a node build.
///
/// A test asserts this against the feature that was compiled; it is the runtime companion of
/// the compile-time split.
#[must_use]
pub const fn is_hub_build() -> bool {
    !cfg!(feature = "node")
}
