//! `spot-nocode-publish` — no-code app packaging and publishing (WP-DX-08.T1, WS21).
//!
//! A designer builds an app without code: DocTypes and forms in `spot-nocode-model`, views,
//! scripts, rules and fixtures from the site's customisations. This crate turns that work into
//! the ordinary, signed `.spotapp` bundle a store installs, and carries it to another tenant.
//!
//! # The pipeline
//!
//! 1. **Package** — [`package`] validates an [`AppDraft`], captures the selected
//!    customisation-type content through `spot-custom-bundle`, writes a `spot-manifest`-valid
//!    declarative manifest and a declarative payload for designed DocTypes, processes and
//!    reports, and packs a deterministic bundle with the empty WebAssembly component. The
//!    result is opened with the **same** `spot-bundle` validator the installer uses and checked
//!    against the host's privilege policy.
//! 2. **Sign** — [`sign`] takes a [`Signer`] that holds the publisher key outside this crate,
//!    then runs `spot-bundle-signing`'s installer-mode verification, including the certificate
//!    chain, validity window and revocation list. The signature binds app id, version, publisher
//!    and canonical digest.
//! 3. **Publish** — [`publish`] runs the `spot-app-review` pipeline first and returns its
//!    report, refuses a blocker, applies the semver rules (no republish, strictly increasing)
//!    and calls `spot-registry`'s publish API through the [`Registry`] seam. A private release
//!    is limited to the certificate's own organisation.
//! 4. **Install** — [`open`] verifies the fetched bytes and reads their payloads;
//!    [`install_plan`] builds the real `spot-installer` plan; [`install`] runs it and then
//!    applies the customisation bundle transactionally to the target tenant. A failed upgrade
//!    is rolled back by the installer and its customisations are not applied.
//!
//! # What a bundle carries
//!
//! Only metadata and declared fixtures. A bundle never carries document rows outside the
//! fixture DocTypes the draft declares, a secret, a private key or a wall-clock reading: the
//! creation time and every registry time are arguments.
//!
//! # Error codes
//!
//! Every refusal is an [`Error`] with a stable `nocode_publish.*` code, catalogued in
//! `crates/framework/spot-errors/catalogue/nocode_publish.toml`.

#![forbid(unsafe_code)]

pub mod codes;
pub mod declarative;
pub mod draft;
pub mod error;
pub mod install;
pub mod package;
pub mod publish;
pub mod sign;

pub use declarative::{CUSTOMISATION_ASSET, DECLARATIVE_ASSET, DeclarativeApp, NamedSource};
pub use draft::{AppDraft, Dependency, ItemRef};
pub use error::Error;
pub use install::{
    InstallRequest, OpenedApp, PLATFORM_WIT_WORLD, apply_customisations, install, install_plan,
    open, remove_app_customisations, remove_customisations, uninstall, uninstall_plan,
};
pub use package::{BundleSource, EMPTY_COMPONENT, package, package_with_policy};
pub use publish::{
    Audience, FetchedBundle, PublishOptions, PublishReceipt, Registry, StoreRegistry,
    check_version_bump, publish, review_app,
};
pub use sign::{SignedSource, Signer, SigningMaterial, sign};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-nocode-publish";
