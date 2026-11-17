//! Stable catalogue codes (`catalogue/sync_skew.toml`). The sync runtime maps these onto the
//! conditions of ADR-067, so the entries carry `class = "none"`.

/// The hub is mid-migration: the node waits and retries.
pub const SESSION_MIGRATING_HUB: &str = "sync_skew.session_migrating_hub";
/// The node is mid-migration: the hub waits and retries.
pub const SESSION_MIGRATING_NODE: &str = "sync_skew.session_migrating_node";
/// The hub's migration epoch is not verified: the node waits and retries.
pub const SESSION_EPOCH_UNVERIFIED_HUB: &str = "sync_skew.session_epoch_unverified_hub";
/// The node's migration epoch is not verified: the hub waits and retries.
pub const SESSION_EPOCH_UNVERIFIED_NODE: &str = "sync_skew.session_epoch_unverified_node";
/// The node is further behind than the allowed skew (`NODE_TOO_OLD`, red).
pub const NODE_TOO_OLD: &str = "sync_skew.node_too_old";
/// An old major cannot be retired because active nodes cannot decode the new one
/// (`SCHEMA_RETIRE_BLOCKED`, amber).
pub const SCHEMA_RETIRE_BLOCKED: &str = "sync_skew.schema_retire_blocked";
/// The hub's per-node hold set is full; the node is reported too old (`NodeStatus::TooOld`).
pub const HOLDS_FULL: &str = "sync_skew.holds_full";
/// An entry's major is retired for the whole fleet: a permanent hold, raised as `NODE_TOO_OLD`.
pub const HOLD_RETIRED: &str = "sync_skew.hold_retired";
/// A persisted hold set names one `(entity, id)` twice; it is refused, never merged.
pub const STATE_DUPLICATE_KEY: &str = "sync_skew.state_duplicate_key";
/// A persisted hub-side queue is not strictly ascending by position; it is refused.
pub const STATE_DISORDER: &str = "sync_skew.state_disorder";
/// A persisted hold set queues more entries than its limit; it is refused.
pub const STATE_OVER_LIMIT: &str = "sync_skew.state_over_limit";
