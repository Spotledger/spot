//! Series keys: the address of a numbering counter.
//!
//! A series key is the resolved prefix of a naming pattern, date parts included, plus an
//! optional scope key (a company, a branch, a till). The date parts are what make a yearly
//! pattern reset: the prefix of `INV-.YYYY-.#####` spells the year, so January gets a fresh
//! key and starts at 1. A pattern with no date part has one key forever.
//!
//! Storage caps a key at 140 bytes ([`SeriesKey::MAX_LEN`]). A longer derived key is replaced
//! by a stable hash with the prefix `h:`: deterministic, printable, and short enough, so a
//! pattern too long to be a legal key still allocates gaplessly from the same counter every
//! time.

use spot_storage_contract::SeriesKey;

use crate::error::NamingError;

/// The prefix of a hashed (too-long) series key.
pub const HASHED_KEY_PREFIX: &str = "h:";

/// Derives the series key for a resolved `prefix` and an optional `scope`.
///
/// # Errors
///
/// [`NamingError::SeriesKeyInvalid`] when the derived key is not legal (for example a prefix
/// that carries a control character and is short enough not to be hashed).
pub fn series_key(prefix: &str, scope: Option<&str>) -> Result<SeriesKey, NamingError> {
    let mut raw = String::with_capacity(prefix.len() + scope.map_or(0, str::len) + 1);
    raw.push_str(prefix);
    if let Some(scope) = scope.filter(|s| !s.is_empty()) {
        raw.push(':');
        raw.push_str(scope);
    }
    // Storage refuses an empty key; a pattern that is only a counter has no prefix of its own.
    if raw.is_empty() {
        raw.push('n');
    }
    if raw.len() > SeriesKey::MAX_LEN {
        raw = format!("{HASHED_KEY_PREFIX}{}", stable_hash_hex(raw.as_bytes()));
    }
    SeriesKey::new(&raw).map_err(|e| NamingError::SeriesKeyInvalid {
        reason: e.explanation,
    })
}

/// A stable, printable 128-bit digest of `bytes`, as 32 lower-case hex characters.
///
/// This is a documented FNV-1a variant (two independent 64-bit lanes), not a cryptographic
/// hash: it only shortens an over-long series key, and it must give the same digest on every
/// build and every machine, which a `DefaultHasher` (unstable across releases) would not.
pub fn stable_hash_hex(bytes: &[u8]) -> String {
    const FNV_OFFSET_A: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_OFFSET_B: u64 = 0x9e37_79b9_7f4a_7c15;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut a = FNV_OFFSET_A;
    let mut b = FNV_OFFSET_B;
    for &byte in bytes {
        a ^= u64::from(byte);
        a = a.wrapping_mul(FNV_PRIME);
        b ^= u64::from(byte).wrapping_add(FNV_OFFSET_B);
        b = b.wrapping_mul(FNV_PRIME);
    }
    format!("{a:016x}{b:016x}")
}
