//! The stable `shared_balance.*` codes of the shared-balance policies.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/shared_balance.toml`. The codes
//! are a **library-internal vocabulary** (class `none`): a decline or a quarantine is an
//! answer, not an `AppError`, and nothing here rides one.

/// An amount was zero or beyond what an integer minor unit can hold.
pub const INVALID_AMOUNT: &str = "shared_balance.invalid_amount";
/// Adding entries overflowed an integer minor unit.
pub const OVERFLOW: &str = "shared_balance.overflow";
/// An entry id was reused for different content.
pub const ENTRY_CONFLICT: &str = "shared_balance.entry_conflict";
/// The balance a caller named does not exist.
pub const UNKNOWN_BALANCE: &str = "shared_balance.unknown_balance";
/// The balance a caller tried to open already exists.
pub const BALANCE_EXISTS: &str = "shared_balance.balance_exists";
/// A quota grant asked for more than the balance has available.
pub const INSUFFICIENT_AVAILABLE: &str = "shared_balance.insufficient_available";
/// A node already holds an open quota grant on the balance.
pub const GRANT_EXISTS: &str = "shared_balance.grant_exists";
/// A policy configuration is not valid.
pub const CONFIG_INVALID: &str = "shared_balance.config_invalid";
/// The quarantine sink could not take a refused entry; the entry is deferred and the sender
/// redelivers it.
pub const SINK_FAILED: &str = "shared_balance.sink_failed";

/// Declined: the node cannot reach head office and the policy needs it.
pub const DECLINED_OFFLINE: &str = "shared_balance.declined_offline";
/// Declined: the balance does not cover the amount.
pub const DECLINED_INSUFFICIENT: &str = "shared_balance.declined_insufficient";
/// Declined: the node holds no quota for the balance.
pub const DECLINED_NO_QUOTA: &str = "shared_balance.declined_no_quota";
/// Declined: the node's quota has expired.
pub const DECLINED_QUOTA_EXPIRED: &str = "shared_balance.declined_quota_expired";
/// Declined: the amount is more than the node's remaining quota.
pub const DECLINED_QUOTA_EXHAUSTED: &str = "shared_balance.declined_quota_exhausted";
/// Declined: accepting would exceed the node's risk cap.
pub const DECLINED_OVER_RISK_CAP: &str = "shared_balance.declined_over_risk_cap";
/// Declined: the node's last view of the balance shows nothing left to spend.
pub const DECLINED_NO_FUNDS: &str = "shared_balance.declined_no_funds";
/// Declined: the amount is not a valid spend.
pub const DECLINED_INVALID_AMOUNT: &str = "shared_balance.declined_invalid_amount";
/// Declined: the node does not know the balance.
pub const DECLINED_UNKNOWN_BALANCE: &str = "shared_balance.declined_unknown_balance";

/// Quarantined: the balance did not cover the entry under a strict policy.
pub const REJECTED_INSUFFICIENT: &str = "shared_balance.rejected_insufficient";
/// Quarantined: the entry has no open quota grant behind it.
pub const REJECTED_NO_GRANT: &str = "shared_balance.rejected_no_grant";
/// Quarantined: the entry would take the node past its granted quota.
pub const REJECTED_OVER_QUOTA: &str = "shared_balance.rejected_over_quota";
/// Quarantined: the entry was made at or after its quota's expiry.
pub const REJECTED_AFTER_EXPIRY: &str = "shared_balance.rejected_after_expiry";
/// Quarantined: the entry names a balance head office does not hold.
pub const REJECTED_UNKNOWN_BALANCE: &str = "shared_balance.rejected_unknown_balance";
/// Quarantined: a node may only send debits.
pub const REJECTED_NOT_PERMITTED: &str = "shared_balance.rejected_not_permitted";
