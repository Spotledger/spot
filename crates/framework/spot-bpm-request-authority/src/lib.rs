//! `spot-bpm-request-authority` — the production request authority (WP-BPM-13.T3, ADR-080).
//!
//! WP-BPM-13.T1 made the request context an opaque **grant** the engine issues and a guard
//! verifies, and left only an in-memory authority for tests and single-site hosts. This crate is
//! the one a deployed site runs.
//!
//! # The grant
//!
//! ```text
//! rg1.<base64url(payload)>.<base64url(tag)>
//! payload = v=1&r=<request>&n=<step>&a=<attempt>&u=<actor>&e=<unix expiry>
//! actor   = user:<id> | consumer:<id> | system   (a typed principal reference, never a bare id)
//! tag     = HMAC-SHA256(secret, "rg1." + base64url(payload))
//! ```
//!
//! The payload names the request, the step, the attempt, the acting person and an **absolute
//! expiry**, and the tag is an HMAC under a server secret resolved from the vault on every call
//! and dropped before the call returns. A grant therefore survives no restart of the secret and
//! leaks nothing: it holds no secret, only a signature over what it is for.
//!
//! [`ProductionRequestAuthority`] is the port the guards already judge a create with
//! ([`spot_bpm_governance::RequestAuthority`]), so turning it on changes no caller. It answers
//! for a grant only when the tag is right (constant-time), the person matches, the expiry has not
//! arrived and the **request store** still holds that step at that attempt. A newer attempt, or a
//! closed request, makes an older grant worthless — including one that leaked.
//!
//! # The edges
//!
//! A grant travels as a command id, so a command id shaped like one is **reserved**:
//! [`is_grant_shaped`] is true for `rg1.…` and for the old plain `request:node:attempt` form, and
//! [`refuse_grant_shaped`] turns that into [`GrantError::CommandIdReserved`]. Edges must refuse
//! such an id, never strip it — a stripped id silently loses idempotency, and a caller that sent
//! a grant as its key has a bug that has to surface.
//!
//! # Purity
//!
//! No storage and no SQL (ADR-045): the [`RequestStore`] is a port the engine implements, and
//! [`MemoryRequestStore`] is its in-memory form. No randomness, no floats, no `unsafe`. Time is
//! an argument ([`ProductionRequestAuthority::issue_until`]) or the injected
//! [`Clock`](spot_core::Clock) port — never a direct wall-clock read in `authenticate`. The
//! secret is never in a field, a `Debug` line, an error or a grant.

#![forbid(unsafe_code)]

pub mod authority;
pub mod codes;
pub mod error;
pub mod grant;
pub mod secret;
pub mod shape;
pub mod store;

pub use authority::{MAX_GRANT_LIFETIME, MIN_SECRET_BYTES, ProductionRequestAuthority};
pub use error::GrantError;
pub use grant::{GRANT_PREFIX, Grant, is_typed_actor};
pub use secret::{FixedSecrets, GrantSecrets, SecretRef};
pub use shape::{is_grant_shaped, refuse_grant_shaped};
pub use store::{MemoryRequestStore, RequestStore};
