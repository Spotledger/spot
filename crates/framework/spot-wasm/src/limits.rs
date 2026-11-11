//! Per-call limits.

use std::time::Duration;

/// What one call may use. Every limit is enforced by the runtime, not by the app's good manners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Units of fuel: roughly one per WebAssembly instruction. Running out is `wasm.out_of_fuel`.
    pub fuel: u64,
    /// Wall-clock time from the start of the call, enforced by epoch interruption in steps of the
    /// host's tick (see [`crate::HostConfig::tick`]). Running out is `wasm.timeout`.
    pub wall_clock: Duration,
    /// Most bytes of linear memory the call may hold. Growing past it is `wasm.memory`.
    pub memory_bytes: usize,
    /// Most elements a table may hold.
    pub table_elements: usize,
    /// Most calls to host imports. The call after the limit is `wasm.too_many_host_calls`.
    pub host_calls: u32,
    /// Largest result, in bytes. A larger one is `wasm.output_too_large`.
    pub output_bytes: usize,
}

impl Default for Limits {
    /// A generous default for trusted-but-fallible code: 100 million fuel, 1 s, 16 MiB, 10,000
    /// table elements, 1,000 host calls, 1 MiB of output.
    fn default() -> Self {
        Self {
            fuel: 100_000_000,
            wall_clock: Duration::from_secs(1),
            memory_bytes: 16 * 1024 * 1024,
            table_elements: 10_000,
            host_calls: 1_000,
            output_bytes: 1024 * 1024,
        }
    }
}
