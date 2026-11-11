#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! Developer debugging and tracing (WP-DX-03).
//!
//! The acceptance sentence of the work package is the design: *breakpoints and traces work across
//! native, WASM and TypeScript code*. Native Rust is debugged with an ordinary debugger; this
//! crate supplies the other two ends and the thread that ties them together:
//!
//! - **[`debug`]**: a Debug Adapter Protocol server (stdio or TCP) for TypeScript scripts run by
//!   `spot-ts`. Breakpoints are set on the TypeScript the author wrote and are mapped to the
//!   generated JavaScript through `spot-ts-transpile`'s source map. The engine exposes no debug
//!   hooks of its own, so the adapter injects line hooks into the transpiled source
//!   ([`debug::instrument`]) and blocks the script's own execution thread at each hook — never
//!   the runtime. `initialize`, `launch`/`attach`, `setBreakpoints`, `continue`, `next`,
//!   `stepIn`, `stepOut`, `pause`, `stackTrace`, `scopes`, `variables`, `evaluate` and
//!   `disconnect` are implemented.
//! - **[`trace`]**: one [`TraceSpan`] tree across http, lifecycle, pipeline steps, scripts, WASM,
//!   storage and jobs, with a per-site ring buffer, querying by request id, redaction on every
//!   attribute and a sink onto the `tracing` pipeline `spot-telemetry` installs.
//! - **[`api`]**: the `/dev/traces` and `/dev/traces/{id}` handler functions and the text the CLI
//!   prints (`spot trace <request-id>`).
//! - **[`wasm`]**: the [`WasmDebugInfo`] boundary view — export, fuel, memory and host calls —
//!   with the DWARF route for native guest step-through documented as out of scope.
//! - **[`safety`]**: a debug session is a development-site capability: no production site, no
//!   release build, a development token, loopback only unless explicitly allowed, and a pause
//!   timeout that resumes a script nobody continued.
//!
//! # No wall clock, ever
//!
//! Span start/end times and the WASM wall time are arguments, measured on the caller's
//! [`spot_core::Clock`]. Nothing in this crate calls `Instant::now`, `SystemTime::now` or
//! `OffsetDateTime::now_utc`. The one blocking wait it owns (a paused script waiting for its
//! debugger) uses the debugger's own pause timeout, not a clock reading.
//!
//! # Where the seams are
//!
//! The host supplies [`debug::DebugHost`]: the closed host surface of the dev site (its document
//! handles and functions) and the production/dev answer for the site. The lead wires
//! [`debug::DebugServer`], [`api::DevApi`] and [`wasm::WasmDebugInfo`] into the dev site's
//! router/CLI; no listener, router or CLI lives here.

pub mod api;
pub mod debug;
pub mod error;
pub mod safety;
pub mod trace;
pub mod wasm;

pub use api::{ApiResponse, DevApi, Format};
pub use debug::{
    DebugEvent, DebugHost, DebugServer, DebugServerConfig, EventSink, LaunchRequest, SessionActor,
};
pub use error::{DevToolsError, Result};
pub use safety::{DEFAULT_DEBUG_ADDRESS, DebugPolicy, DevToken};
pub use trace::{
    ActiveSpan, Layer, SAMPLE_ALL_PPM, SpanId, TelemetrySink, Trace, TraceCollector, TraceContext,
    TraceId, TraceSink, TraceSpan,
};
pub use wasm::{DWARF_NOTE, HostCall, WasmCallReport, WasmDebugInfo};
