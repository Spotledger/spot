//! The size and count limits a read enforces.

/// The bounds [`Bundle::open`](crate::Bundle::open) enforces while reading. Defaults:
/// 64 MiB compressed, 256 MiB total uncompressed, 4096 entries, 128 MiB one file.
///
/// Every bound is enforced against the bytes that are **actually** read — the inflated size
/// is measured while decompressing, never taken from the archive's own declarations, so a
/// zip bomb dies early without allocating its full size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The largest input, in bytes (the archive as stored): 64 MiB.
    pub max_compressed_bytes: u64,
    /// The largest total inflated size of all entries together: 256 MiB.
    pub max_total_bytes: u64,
    /// The largest number of entries, counting files and directory entries: 4096.
    pub max_entries: u32,
    /// The largest inflated size of one entry: 128 MiB.
    pub max_file_bytes: u64,
}

impl Limits {
    /// The default limits.
    pub const fn new() -> Self {
        Self {
            max_compressed_bytes: 64 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_entries: 4096,
            max_file_bytes: 128 * 1024 * 1024,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}
