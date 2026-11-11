//! Spotledger app packaging (WP-EXT-07.T1): an app directory — manifest, DocTypes,
//! permissions, optional WASM component, assets — validated end to end, packed into one
//! deterministic bundle, signed with a publisher key, and verified round trip.
//!
//! The pieces below it exist separately; this crate runs them in the one order that gives an
//! author a single answer: "this app is valid, here is the bundle, here is its digest".
//!
//! # Shape
//!
//! - [`check_app`] -> [`CheckReport`]: every problem at once, each with its file, in stable
//!   `pack.*` codes.
//! - [`pack_app`] -> [`PackedBundle`]: the checked app through `spot-bundle`'s
//!   [`BundleBuilder`], deterministic bytes (sorted entries, fixed timestamps, no host
//!   metadata) and the canonical digest.
//! - [`signing`]: signing through `spot-bundle-signing`, the key from a
//!   [`KeyProvider`](signing::KeyProvider) (a file or an injected implementation — never an
//!   environment variable holding key material), and the one verify the installer calls.
//! - [`report`]: the human text and JSON forms of a check or an inspect.
//! - `main.rs`: the `spot-pack` command line (`check`, `pack`, `verify`, `inspect`,
//!   `--json`, exit codes 0 ok / 1 problems / 2 usage, no network).
//!
//! # What this crate does not do
//!
//! Choose capability policy (the host's decision), read a clock, touch the network, or edit
//! the crates it drives.

#![forbid(unsafe_code)]

pub mod check;
pub mod error;
pub mod imports;
pub mod inspect;
pub mod layout;
pub mod pack;
pub mod pathsafe;
pub mod report;
pub mod signing;
pub mod wasm_component;

pub use crate::check::{CheckReport, ControllerKind, ControllerRef, check_app};
pub use crate::error::{PackError, PackProblem, VerifyRefusal};
pub use crate::pack::{PackOptions, PackedBundle, pack_app};
pub use crate::signing::{
    BundleProof, FileKeyProvider, KeyProvider, install_policy, sign_packed, verify_packed,
    write_key_file,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-pack";
