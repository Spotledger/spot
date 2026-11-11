//! Adapters from the hub relay to the sync policy crates (WP-SYN-02.T8, ADR-063 section 3).
//!
//! `spot-sync-hub` asks a `DestinationScope` whether a destination may receive a stream entry.
//! The scope rules live in `spot-sync-master` (`Subscription::covers`). [`MasterScope`] joins
//! the two without making the hub core depend on the master crate.
//!
//! # What comes from the hub, what from the node
//!
//! * From the hub registry: the destination's `Subscription` (entities and scope keys it is
//!   entitled to) and, through [`ScopeKeysOf`], the scope keys each record requires.
//! * From the node: **nothing**. No field a node sent is read here, so a node cannot widen its
//!   own scope.
//!
//! # Fail closed
//!
//! An entry is covered only when everything needed to decide is known and valid: a valid entity
//! and record name, a version of at least 1, and known scope keys that all parse. Anything else
//! answers "not covered", never a panic. Master data is hub-authored and flows down only, so
//! `DestinationScope::owns` is always `false`.

#![warn(missing_docs)]

mod keys;
mod redact;
mod scope;

pub use keys::{ScopeKeysOf, decode_scope_key, encode_scope_key};
pub use redact::{PolicyBook, PolicyRedactor};
pub use scope::MasterScope;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-hub-adapters";
