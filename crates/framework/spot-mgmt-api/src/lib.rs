//! The management API server library (`WP-ADM-01.T4`, ADR-44, ADR-097).
//!
//! `router(deps)` is the axum router of `contracts/management-api/openapi.yaml`;
//! [`tls::tls_config`] and [`tls::serve`] give it a TLS 1.3 mutual-TLS listener whose client
//! certificates must chain to the operator CA. The crate is a library: the host module that binds
//! a port and a PostgreSQL nonce/command store is `WP-ADM-01.T5`, the real operations
//! `WP-ADM-01.T6`.
//!
//! # Security shape
//!
//! Default deny. Nothing is reachable without a verified client certificate that is on the
//! operator allow-list; an operation above `read` needs a fresh signed envelope for this site;
//! the nonce is consumed (durably, fail closed) before any operation runs. There is no token or
//! cookie authentication. Error bodies carry fixed text and at most a parameter *name*. Every
//! request that reaches authentication is audited, refusals included, before the response is
//! sent; when the audit sink fails, an operation above `read` answers `mgmt.internal` and does
//! not run. Time is read only through the injected [`spot_core::Clock`].

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod app;
pub mod error;
pub mod handlers;
mod invoke;
pub mod lifecycle;
pub mod ports;
mod routes;
pub mod stubs;
pub mod tls;
pub mod view;
pub mod wire;

pub use app::{Deps, MAX_BODY_BYTES, MAX_CLOCK_SKEW_SECS, MAX_COMMAND_LIFETIME_SECS};
pub use error::{MgmtCode, MgmtError};
pub use routes::router;
pub use tls::tls_config;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-mgmt-api";
