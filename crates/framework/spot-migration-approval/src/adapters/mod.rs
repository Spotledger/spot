//! Real implementations of the approval service's outside traits (WP-MIG-04.T2), each behind
//! its own feature so the default crate stays light:
//!
//! | feature          | adapter                | over                  |
//! |------------------|------------------------|-----------------------|
//! | `sod`            | `SodAuthorship`        | `spot-sod`            |
//! | `signed-command` | `SignedCommandHook`    | `spot-signed-command` |
//! | `settings`       | `SettingsPolicySource` | `spot-settings`       |
//! | `audit-chain`    | `ChainAuditSink`       | `spot-audit-chain`    |
//!
//! Every adapter fails closed: when it cannot decide, the answer is the one that blocks.

#[cfg(feature = "audit-chain")]
mod chain;
#[cfg(feature = "settings")]
mod settings;
#[cfg(feature = "signed-command")]
mod signed;
#[cfg(feature = "sod")]
mod sod;

#[cfg(feature = "audit-chain")]
pub use chain::ChainAuditSink;
#[cfg(feature = "settings")]
pub use settings::{PolicySource, SettingsPolicySource, strictest_policy};
#[cfg(feature = "signed-command")]
pub use signed::{APPROVE_OP, ApproverKey, SignedCommandHook, approval_params, encode_signature};
#[cfg(feature = "sod")]
pub use sod::{APPROVE_DUTY, AUTHOR_DUTY, SodAuthorship};
