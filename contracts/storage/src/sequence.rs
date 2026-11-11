//! Gapless numbering series (ADR-46; used by WP-DOC-05 naming).

use std::future::Future;

use crate::error::{ErrorKind, StorageError, StorageResult};

/// Identifies a numbering series, for example `SINV-2026-` or `pos:branch7:till2`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesKey(String);

impl SeriesKey {
    /// Longest allowed key in bytes.
    pub const MAX_LEN: usize = 140;

    /// Validates a key: 1 to 140 bytes of printable text.
    pub fn new(key: &str) -> StorageResult<Self> {
        if key.is_empty() || key.len() > Self::MAX_LEN || key.chars().any(char::is_control) {
            return Err(StorageError::new(
                ErrorKind::InvalidValue,
                format!("series key must be 1 to {} printable bytes", Self::MAX_LEN),
            ));
        }
        Ok(Self(key.to_owned()))
    }

    /// The key as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Gapless sequence allocation.
///
/// Numbers are allocated inside the transaction: if it rolls back, the numbers return to the
/// series, so committed numbers have no gaps and no duplicates (a checked invariant, blueprint
/// §3). Allocation serializes on the series: a second transaction allocating from the same
/// series waits until the first ends. Keep such transactions short.
pub trait Sequences: Send {
    /// Allocates `count` consecutive numbers and returns the first. A series that was never used
    /// starts at 1. A `count` of 0 fails with `InvalidValue`.
    fn allocate(
        &mut self,
        series: &SeriesKey,
        count: u32,
    ) -> impl Future<Output = StorageResult<u64>> + Send;

    /// Sets the next number a series will hand out (setup and migration only). Fails with
    /// `InvalidValue` if it would reuse a number already allocated.
    fn set_next(
        &mut self,
        series: &SeriesKey,
        next: u64,
    ) -> impl Future<Output = StorageResult<()>> + Send;
}
