//! `spot-store-server`: the Spotledger app store as a service (WP-STR-03.T3, ADR-21).
//!
//! It serves `contracts/store-api` over the registry core (`spot-registry`), its PostgreSQL index
//! (`spot-registry-pg`) and the store's own durable state (`spot-store-state-pg`): publisher key
//! authentication, publish with the registry's signature, revocation and monotonic-version
//! checks, a review hand-off, search, bundle download with `ETag` and range, yank, and the
//! signed revocation list. Nothing here signs a bundle or a certificate; the only key is the
//! store key, which signs revocation lists (and, later, licences).
//!
//! # The trust model
//!
//! Sites verify every bundle themselves, so the signature chain is the only trust. The service
//! still refuses bad input early: it checks sizes and content types before reading a body, never
//! uses a request value as a path (blob paths come from the content digest), compares publisher
//! keys only as SHA-256 in constant time, and keeps no key in a log, an error or an audit entry.
//!
//! # The listing fails closed
//!
//! A version is public only with an explicit `listed` row in the state store. A published
//! version starts as `pending_review`; a person approves it through an operator route
//! (`/admin/v1/...`, behind a second credential). The review report never says an app is safe.
//!
//! # Blocking calls (ADR-082 class C)
//!
//! The registry, the PostgreSQL index and the state store are synchronous, and the shared bridge
//! refuses any caller inside a tokio runtime (`spawn_blocking` threads included). This service
//! therefore owns a small pool of plain OS threads fed by a bounded queue; a handler sends a
//! closure and awaits a oneshot reply, and a full queue is `503` with `Retry-After`. It is a
//! control-plane service and a separate deployable, never the tenant request path. No handler
//! thread blocks, and no `std::sync` guard is held across an await.
//!
//! # Time
//!
//! The only reader of the wall clock is [`SystemClock`], behind the [`Clock`] trait.
//!
//! See `docs/running-spot/store-server.md` for the configuration and the operator routes.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod admin;
mod auth;
mod bootstrap;
mod clock;
mod config;
mod dynstore;
mod error;
mod exec;
mod handlers;
mod http;
mod keys;
mod metrics;
mod ops;
mod pool;
mod publish;
mod ratelimit;
mod review;
mod revocation;
mod router;
mod service;

pub use bootstrap::{Deployment, build_deployment, run_cli};
pub use clock::{Clock, ManualClock, SystemClock};
pub use config::{
    CONTRACT_MAX_BUNDLE_BYTES, CONTRACT_MAX_MANIFEST_BYTES, Config, DatabaseConfig, Limits, Rates,
    ReviewSection, RevocationConfig, Rule, StartupError, TrustRootConfig, Workers,
};
pub use error::codes;
pub use keys::{
    AdminCredential, StoreKey, ct_compare_count, ct_eq, hash_key, is_publisher_token, sha256_bytes,
};
pub use router::{router, serve};
pub use service::{Service, ServiceParts};
