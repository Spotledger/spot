#![forbid(unsafe_code)]
//! The WebAssembly component host (WP-EXT-05.T1; ADR-16, ADR-57).
//!
//! WASM apps run here with hard resource limits and **no ambient authority**. The acceptance
//! sentence of the work package is the design: *a sample app installs, runs, and is killed on
//! timeout without affecting the runtime.*
//!
//! # Shape
//!
//! - [`WasmHost`]: one `wasmtime` engine (component model, fuel, epoch interruption, a pooling
//!   allocator with caps) and one ticker thread for every wall-clock limit.
//!   [`WasmHost::compile`] validates a component and hashes it; [`WasmHost::instantiate`] links it
//!   for an app that was granted [`GrantedCapabilities`].
//! - [`AppInstance::call`]: **a fresh instance from the pool per call**, run under [`Limits`]
//!   (fuel, wall clock, memory, table elements, host calls, output size). Nothing survives between
//!   calls. Whatever happens inside, the slot returns to the pool and other calls are unaffected.
//! - **Capabilities.** The linker provides only the imports an app's granted capabilities allow
//!   ([`capabilities`]). An app that imports anything else fails to link with
//!   `wasm.capability_denied` naming the import; nothing from WASI (files, sockets, clocks,
//!   environment) is available, and time is the invocation's (`now`), never the machine's.
//! - [`WasmError`]: typed errors, each with a stable `wasm.*` code.
//!
//! # The world
//!
//! `wit/test-world.wit` is the **test** world the host and its fixtures are proven against: an
//! export `on-event(event: string) -> result<string, error>` and the imports `log`, `now`,
//! `get-doc`. The product world (WP-EXT-05.T2) replaces it; the host's import table
//! ([`capabilities`]) is where its imports are added.
//!
//! # Arguments and results
//!
//! Calls take and return JSON, converted by the export's declared types ([`json`] documents the
//! mapping). This keeps the host independent of any particular world.
//!
//! # What this crate does not do
//!
//! Choose which events reach which app, read manifests, persist anything, or define the product
//! WIT world.

pub mod capabilities;
pub mod error;
pub mod json;

mod app;
mod hash;
mod host;
mod imports;
mod limits;

pub use app::AppInstance;
pub use capabilities::{DocTypeAccess, Endpoint, FileAccess, GrantedCapabilities};
pub use error::{Result, WasmError};
pub use host::{CompiledComponent, HostConfig, WasmHost};
pub use imports::{Imports, LogLevel};
pub use limits::Limits;
