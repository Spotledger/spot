//! `spot-pg-bridge`: the one shared, bounded way for synchronous code to reach PostgreSQL
//! (ADR-082 addendum 1, WP-CON-01.T5).
//!
//! # When a bridge is legitimate, and when it is a defect
//!
//! 1. **No sync-over-async on a request path.** Code that runs inside a request, a hook, a
//!    lifecycle step, a job handler or a realtime/SSE task never blocks on a database future.
//!    If it needs data, the async layer loads it first and the synchronous code receives it as
//!    an argument. A rule author who must read mid-step uses `HookContext`; a rule never opens
//!    a bridge. Calling this crate from such code is a defect, and the call is refused with
//!    `pg_bridge.wrong_thread` when the thread belongs to a tokio runtime.
//! 2. **Where a bridge is legitimate** (control-plane processes, CLI commands, migrations, the
//!    class C stores of ADR-082) there is exactly one, constructed once per process and shared
//!    by `Arc`. No `*-pg` crate writes its own `block_on`.
//!
//! # How a call runs
//!
//! [`Bridge::run`] takes a deadline and a closure that receives a pooled connection and returns
//! a future. The caller's thread blocks on a plain channel; the future runs on the bridge's own
//! runtime. In order:
//!
//! - the caller's thread must not be inside any tokio runtime (`wrong_thread`);
//! - the call is admitted only if fewer than `max_in_flight + queue_capacity` calls are
//!   outstanding (`busy`, at once, nothing queued);
//! - it waits for one of `max_in_flight` slots, then for a pool connection, then runs;
//! - at the deadline (which includes the waiting) the statement is cancelled through the
//!   driver's cancel request, the connection is closed rather than pooled, and the caller gets
//!   `timeout`.
//!
//! The store's own error comes back as [`BridgeError::Store`], never as a default value.
//!
//! # Cancel and TLS
//!
//! [`Bridge::for_backend`] is the normal construction: it takes the backend's pool and, when the
//! pool uses TLS, the same connector for the cancel request, so the cancel key never travels in
//! clear and a host cannot forget it. A caller that has only a pool uses [`Bridge::new`] (plain)
//! or [`Bridge::with_cancel_tls`] (its own connector).
//!
//! # Session state
//!
//! The pool recycles connections with `RecyclingMethod::Fast`, so a connection comes back to the
//! next caller exactly as the previous one left it: the bridge **resets nothing between calls**.
//! `SET search_path`, `SET ROLE`, `SET statement_timeout`, a temporary table, a session advisory
//! lock or a named prepared statement made inside a closure outlives the call and reaches the
//! next caller of this bridge, or of the shared pool. Scope settings with `SET LOCAL` inside a
//! transaction. See the crate README, "Session state".

mod config;
mod core;
mod error;
pub mod metrics;
mod pg;

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_postgres::PgBackend;
use tokio_postgres::Socket;
use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};

pub use crate::config::{BridgeConfig, RESERVED_CONNECTIONS};
pub use crate::core::BoxFuture;
pub use crate::error::{BUSY, BridgeError, CLOSED, CONFIG, CONNECT, TIMEOUT, WRONG_THREAD};
pub use crate::pg::PooledConn;

use crate::core::Core;
use crate::pg::{CancelWith, NoTlsCancel, PgSource, TlsCancel};

/// The shared blocking bridge. Construct one per process and share it by `Arc`.
pub struct Bridge {
    core: Core<PgSource>,
}

impl Bridge {
    /// Starts a bridge on `pool` with cancel requests sent without TLS.
    ///
    /// The pool's connections are opened on the bridge's runtime, so build the pool lazily
    /// (no connection opened before the first call), as `PgBackend::connect_unchecked` does.
    /// A host that holds the [`PgBackend`] uses [`Bridge::for_backend`] instead, which also
    /// matches the cancel request to the pool's transport.
    ///
    /// # Errors
    ///
    /// [`BridgeError::Config`] naming the first setting that cannot hold against the pool's
    /// size (see [`BridgeConfig::validate`]).
    pub fn new(config: BridgeConfig, pool: Pool) -> Result<Bridge, BridgeError> {
        Self::build(config, pool, Arc::new(NoTlsCancel))
    }

    /// As [`Bridge::new`], sending cancel requests with `tls`, the connector the pool uses.
    ///
    /// # Errors
    ///
    /// As [`Bridge::new`].
    pub fn with_cancel_tls<T>(
        config: BridgeConfig,
        pool: Pool,
        tls: T,
    ) -> Result<Bridge, BridgeError>
    where
        T: MakeTlsConnect<Socket> + Clone + Send + Sync + 'static,
        T::Stream: Send,
        T::TlsConnect: Send,
        <T::TlsConnect as TlsConnect<Socket>>::Future: Send,
    {
        Self::build(config, pool, Arc::new(TlsCancel(tls)))
    }

    /// Starts a bridge on `backend`'s pool, sending cancel requests over the transport that pool
    /// uses: with the backend's own TLS connector when its config requires TLS
    /// ([`PgBackend::cancel_tls`]), and without TLS when it does not.
    ///
    /// This is the normal construction for a host that holds its [`PgBackend`]: one call, the
    /// backend's pool, and no way to forget the connector that keeps a cancel key out of clear.
    ///
    /// # Errors
    ///
    /// As [`Bridge::new`], checked against the backend's pool size.
    pub fn for_backend(config: BridgeConfig, backend: &PgBackend) -> Result<Bridge, BridgeError> {
        let pool = backend.pool();
        match backend.cancel_tls() {
            Some(tls) => Self::with_cancel_tls(config, pool, tls),
            None => Self::new(config, pool),
        }
    }

    /// The one place a bridge is built, so every constructor validates and starts it the same way.
    fn build(
        config: BridgeConfig,
        pool: Pool,
        cancel: Arc<dyn CancelWith>,
    ) -> Result<Bridge, BridgeError> {
        config.validate(pool.status().max_size)?;
        let source = PgSource { pool, cancel };
        Ok(Bridge {
            core: Core::start(config, source)?,
        })
    }

    /// Test hook: whether this bridge sends cancel requests over TLS.
    #[cfg(test)]
    fn cancel_uses_tls(&self) -> bool {
        self.core.source().cancel_uses_tls()
    }

    /// Runs `op` on a pooled connection and blocks the calling thread until it finishes, fails
    /// or `deadline` passes. A deadline above `max_deadline` is clamped, not refused.
    ///
    /// # Session state
    ///
    /// The bridge resets nothing between calls, and the pool recycles connections with
    /// `RecyclingMethod::Fast`: whatever the closure does to the session stays on the connection
    /// and reaches the next caller of this bridge, or of the shared pool. That includes a
    /// session-level `SET` (`search_path`, `ROLE`, `statement_timeout`), a temporary table, a
    /// session advisory lock and named prepared statements. Scope settings with `SET LOCAL` (or
    /// `set_config(name, value, true)`) inside a transaction; do not create temporary tables or
    /// take session locks here. See the crate README, "Session state".
    ///
    /// # Errors
    ///
    /// [`BridgeError::WrongThread`], `Busy`, `Timeout`, `Closed`, `Connect` or the store's own
    /// error as `Store`. See the crate documentation.
    pub fn run<T, E, F>(&self, deadline: Duration, op: F) -> Result<T, BridgeError<E>>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: FnOnce(PooledConn) -> BoxFuture<'static, Result<T, E>> + Send + 'static,
    {
        self.core.run(deadline, op)
    }

    /// [`Bridge::run`] with the configured `default_deadline`.
    ///
    /// # Errors
    ///
    /// As [`Bridge::run`].
    pub fn run_default<T, E, F>(&self, op: F) -> Result<T, BridgeError<E>>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: FnOnce(PooledConn) -> BoxFuture<'static, Result<T, E>> + Send + 'static,
    {
        self.core.run(self.core.config().default_deadline, op)
    }

    /// Stops accepting calls, waits up to `grace` for those in flight, then cancels the rest
    /// and releases the runtime. Returns within `grace` plus a short settle time for the
    /// cancel requests. Later calls return `Closed`.
    ///
    /// # Errors
    ///
    /// [`BridgeError::WrongThread`] when called from inside a tokio runtime (it blocks).
    pub fn shutdown(&self, grace: Duration) -> Result<(), BridgeError<Infallible>> {
        self.core.shutdown(grace)
    }

    /// The configuration this bridge was built with.
    #[must_use]
    pub fn config(&self) -> &BridgeConfig {
        self.core.config()
    }
}

#[cfg(test)]
mod tests;
