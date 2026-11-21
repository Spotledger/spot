//! Per-execution limits.

use std::time::Duration;

/// What one execution may use. Every limit is enforced by the engine, not by the script's manners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The runtime's memory limit (`set_memory_limit`). Default 32 MiB.
    pub memory_bytes: usize,
    /// The native stack the script may use (`set_max_stack_size`). Default 256 KiB.
    pub stack_bytes: usize,
    /// Wall-clock deadline from the start of the execution, checked by the interrupt handler
    /// about every 10,000 instructions. Default 1 s.
    pub wall_clock: Duration,
    /// An instruction budget, in instructions, counted in steps of the interrupt handler's period
    /// (about 10,000), so it is approximate to that granularity. Default 1 billion.
    pub instructions: u64,
    /// Most calls to host functions. Default 1,000.
    pub host_calls: u32,
    /// Largest serialised result, in bytes. Default 1 MiB.
    pub output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 32 * 1024 * 1024,
            stack_bytes: 256 * 1024,
            wall_clock: Duration::from_secs(1),
            instructions: 1_000_000_000,
            host_calls: 1_000,
            output_bytes: 1024 * 1024,
        }
    }
}
