#![forbid(unsafe_code)]
//! The connector SDK and host (WP-INT-05.T1; blueprint WS7, ADR-49).
//!
//! A **connector** is a WebAssembly component on the same sandbox as an app (`spot-wasm`):
//! it has no network, no filesystem and no clock of its own, and it is stopped by fuel, a
//! wall-clock deadline and a memory cap. It exports the `connector` world
//! ([`wit/connector.wit`](https://example.invalid/connector.wit)):
//!
//! - `describe() -> ConnectorInfo` — its name, version, operations (JSON Schema in and out) and
//!   auth kinds;
//! - `invoke(op, input, ctx) -> result<output, error>` — one operation;
//! - `webhook(request) -> result<response, error>` — an inbound request.
//!
//! and imports only what the host provides: `log`, `http-request` (through the egress policy),
//! `secrets-get` (only a reference it declared), `kv-get` and `kv-set`.
//!
//! # Shape
//!
//! - [`ConnectorHost`]: loads a signed bundle ([`SignedBundle`]), describes it, invokes its
//!   operations and delivers webhooks, through the credential [`Vault`], the monitor and the
//!   per-connector circuit breaker. It implements [`ConnectorPort`], the trait the
//!   business-process engine drives.
//! - [`ConnectorRuntime`]: the `wasmtime` engine (component model, fuel, epoch interruption, a
//!   pooling allocator), configured exactly like `spot-wasm`'s, with one fresh store and
//!   instance per call.
//! - [`Vault`] and [`AuthSpec`]: credential references resolved at call time. For `bearer`,
//!   `basic` and `oauth2` the host injects the credential into the outbound request, so the
//!   guest never holds the token; OAuth2 tokens are cached in memory and refreshed once under
//!   concurrency.
//! - [`MappingSpec`]: source path to target path through `spot-expr`, with type coercion —
//!   the same mapping the BPM integration node uses.
//! - [`ConnectorMonitor`]: bounded-label counters and latency histograms (through
//!   `spot-telemetry`), a last-error record and a per-connector, per-tenant circuit breaker.
//! - [`testkit`]: a mock host with recorded HTTP exchanges and the mapping checker, for
//!   connector authors (`testkit` feature).
//!
//! # The SDK
//!
//! `sdk/connector-template/` is the Rust guest template: a `wit-bindgen` crate, a
//! `connector.toml` and a sample connector under `examples/echo-http/`. A connector built from
//! the template alone runs on [`ConnectorHost`].
//!
//! # What this crate does not do
//!
//! Choose the egress policy (the guard's decision), read the wall clock (time is an argument),
//! persist a credential (the vault seals it), or give a guest raw network or filesystem access.

pub mod config;
pub mod error;
pub mod host;
pub mod imports;
pub mod mapping;
pub mod model;
pub mod monitor;
pub mod port;
pub mod runtime;
pub mod vault;

#[cfg(feature = "testkit")]
pub mod testkit;

mod bindings;

pub use crate::config::ConnectorConfig;
pub use crate::error::{ConnectorError, FailureClass, Result, codes};
pub use crate::host::{ConnectorHost, SignedBundle};
pub use crate::imports::{
    CallServices, EgressHttp, HttpContext, HttpService, InMemoryKv, KvStore, LogSink, NullLogSink,
    RecordingLogSink,
};
pub use crate::mapping::{Coercion, MappingReport, MappingRule, MappingSpec, check_mapping};
pub use crate::model::{
    AuthKind, CallContext, ConnectorId, ConnectorInfo, HttpRequest, HttpResponse, Operation,
};
pub use crate::monitor::{BreakerState, ConnectorMonitor, MonitorConfig, Refusal, Stats};
pub use crate::port::{ConnectorCall, ConnectorHealth, ConnectorOutput, ConnectorPort};
pub use crate::runtime::ConnectorRuntime;
pub use crate::vault::{
    AuthSpec, CredentialRef, CredentialVault, OAuth2Config, SealedVault, TokenTransport, Vault,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-connectors";
