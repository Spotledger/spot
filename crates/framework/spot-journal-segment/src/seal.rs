//! The sealing policy: a pure function of the open segment's state, a time argument and limits.

/// What the relay knows about the open segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealState {
    /// Entries appended so far.
    pub entries: u64,
    /// Uncompressed bytes appended so far.
    pub bytes: u64,
    /// When the first entry was appended (milliseconds since the epoch, supplied by the caller).
    pub opened_at_millis: i64,
}

/// When to seal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealLimits {
    /// Seal at or above this many uncompressed bytes.
    pub max_bytes: u64,
    /// Seal when the segment has been open at least this long.
    pub max_age_millis: u64,
}

impl Default for SealLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_age_millis: 5 * 60 * 1000,
        }
    }
}

/// Whether the open segment should be sealed now. An empty segment never seals, however old:
/// an idle tenant never produces an empty segment.
#[must_use]
pub fn should_seal(state: &SealState, now_millis: i64, limits: &SealLimits) -> bool {
    if state.entries == 0 {
        return false;
    }
    if state.bytes >= limits.max_bytes {
        return true;
    }
    let age = now_millis.saturating_sub(state.opened_at_millis);
    // A clock that went backwards gives a negative age, which never seals by time.
    u64::try_from(age).is_ok_and(|a| a >= limits.max_age_millis)
}
