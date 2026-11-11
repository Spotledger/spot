//! Signed app registry core (WP-STR-03). No network or implicit clock.
//!
//! Storage digests are **bundle canonical digests**, not hashes of ZIP bytes. Stores
//! validate through `spot-bundle`; signing always delegates to `spot-bundle-signing`.
//! Publish uses Install verification (the signing API has no Publish mode). Index
//! implementations must serialize mutation checks and commits across shared registries.
//! The filesystem adapter performs blocking local I/O within boxed async operations;
//! a server should dispatch it on its blocking executor. No runtime is required here.
#![forbid(unsafe_code)]

mod audit;
mod blob;
mod client;
mod error;
mod index;
mod model;
mod registry;
mod search;
mod transfer;

pub use audit::*;
pub use blob::*;
pub use client::*;
pub use error::*;
pub use index::*;
pub use model::*;
pub use registry::*;
pub use search::*;
pub use semver::{Version, VersionReq};
pub use spot_bundle::Digest;
pub use transfer::*;
