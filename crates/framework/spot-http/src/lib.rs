//! HTTP server core (WP-PLT-01.T1): the axum/hyper edge every backend service mounts its routes
//! into. See blueprint §2 (wave 1) and the task design notes.
//!
//! Contents:
//! - [`config::ServerConfig`]: bounds and addresses, serde-loadable,
//! - [`server::ServerBuilder`]: assembles the router with limits, timeouts, request ids and the
//!   health endpoints, binds the listeners and accepts with graceful shutdown,
//! - [`server::ServerHandle`]: live connection counter and join,
//! - [`site::StaticSites`]/[`site::SiteResolver`]: host-to-site resolution (ADR-34),
//! - [`request::RequestId`]: end-to-end request identification,
//! - [`error::ErrorEnvelope`]: the native API error envelope (WP-CON-03, ADR-055).
//!
//! Resource and method endpoints are mounted by WP-PLT-05, authentication by WP-PLT-02. This
//! crate only wires the shared HTTP core.

pub mod api;
pub mod config;
pub mod error;
pub mod health;
pub mod middleware;
pub mod request;
pub mod server;
pub mod site;

pub use config::ServerConfig;
pub use error::{ErrorEnvelope, HttpErrorCode, ServerError};
pub use health::{CheckResult, ReadinessCheck};
pub use request::RequestId;
pub use server::{ServerBuilder, ServerHandle};
pub use site::{SiteContext, SiteResolver, StaticSites};

/// Test-only unwrapping that names the failure (the workspace denies `unwrap`).
#[cfg(test)]
pub(crate) trait Must<T> {
    /// Unwraps or panics with the underlying error.
    fn must(self) -> T;
}

/// Test-only unwrapping of the `Err` side.
#[cfg(test)]
pub(crate) trait MustErr<E> {
    /// The `Err` value, panicking when the result is `Ok`.
    fn must_err(self) -> E;
}

#[cfg(test)]
impl<T, E: std::fmt::Debug> Must<T> for Result<T, E> {
    fn must(self) -> T {
        self.unwrap_or_else(|e| panic!("unexpected error: {e:?}"))
    }
}

#[cfg(test)]
impl<T: std::fmt::Debug, E> MustErr<E> for Result<T, E> {
    fn must_err(self) -> E {
        match self {
            Ok(v) => panic!("unexpected Ok: {v:?}"),
            Err(e) => e,
        }
    }
}

#[cfg(test)]
impl<T> Must<T> for Option<T> {
    fn must(self) -> T {
        self.unwrap_or_else(|| panic!("unexpected None"))
    }
}
