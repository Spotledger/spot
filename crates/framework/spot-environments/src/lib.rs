//! Environments (WP-ALM-02, blueprint WS19, ADR-22): development, test and production as linked
//! tenants, and the transport that carries a customization bundle between them.
//!
//! The pieces:
//! - [`model`]: [`EnvironmentSet`] — a name, a shared app set and region class, and an ordered
//!   ladder of [`Environment`] members (dev -> test -> prod). Linking checks each member's
//!   tenant through the control store. A skip is refused unless [`Policy::allow_skip`] is set.
//! - [`store`]: [`EnvironmentStore`], the environment side of the control store, with unique
//!   names and optimistic versions, and [`MemoryEnvironmentStore`] for tests.
//! - [`transport`]: [`transport`] packs a bundle and a signed envelope (source, target, digest,
//!   bundle version, creator, signature, certificate). [`TransportPackage::verify`] checks the
//!   digest and the signature before the route, and refuses an unknown signer.
//! - [`variables`]: `Variables` resolve `${env.NAME}` placeholders at apply time. The
//!   placeholder text is inside the content digest, the values are not, so one bundle keeps one
//!   digest across environments while the applied values differ. Secret values never travel:
//!   a secret is a reference only.
//! - [`promotion`]: [`PromotionRecord`] appended to a hash-linked [`PromotionChain`], and
//!   [`import`], which verifies, resolves, plans, applies, captures the target back and records
//!   the result. [`verify_chain`] proves one digest traversed dev -> test -> prod with an
//!   identical result at every hop.
//!
//! This crate is the transport and environment model. The promotion *pipeline* with approvals
//! and rehearsal is WP-ALM-04. Nothing here reads a clock, a random source or the network:
//! `signed_at` and `at` are arguments, and signing keys are supplied by the caller.

#![forbid(unsafe_code)]

pub mod error;
pub mod model;
pub mod promotion;
pub mod store;
pub mod transport;
pub mod variables;

pub use error::{EnvironmentError, Result, codes};
pub use model::{AppRef, Environment, EnvironmentSet, Policy, Role};
pub use promotion::{
    ChainProof, Context, Imported, PromotionChain, PromotionRecord, Site, import, verify_chain,
};
pub use store::{EnvironmentStore, MemoryEnvironmentStore, StoredSet};
pub use transport::{
    EnvRef, Envelope, Signer, TransportPackage, Trust, VerifiedPackage, transport,
};
pub use variables::{Binding, SecretRef, Variables};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-environments";
