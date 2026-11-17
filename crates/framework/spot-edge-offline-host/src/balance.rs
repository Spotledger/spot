//! The shared-balance policy gate. The default is the provisional `OnlineOnly`; this crate never
//! picks another default (the architecture owner's business decision is still open).

use std::fmt;

/// The stable code of an offline spend refused by `OnlineOnly`.
pub const BALANCE_ONLINE_ONLY: &str = "edge_offline.balance_online_only";

/// How a balance may be spent while the node is offline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BalancePolicy {
    /// Never spent offline (the default).
    #[default]
    OnlineOnly,
    /// Spent against a quota allocated before going offline.
    AllocatedQuota,
    /// Accepted offline and reconciled on reconnect.
    AcceptAndReconcile,
}

impl BalancePolicy {
    /// Parses `online-only`, `allocated-quota` or `accept-and-reconcile`; anything else is
    /// `OnlineOnly` (fail closed).
    pub fn parse(text: &str) -> Self {
        match text.trim() {
            "allocated-quota" => Self::AllocatedQuota,
            "accept-and-reconcile" => Self::AcceptAndReconcile,
            _ => Self::OnlineOnly,
        }
    }
}

/// An offline spend was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnlineOnly;

impl fmt::Display for OnlineOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{BALANCE_ONLINE_ONLY}: this balance can only be spent while connected to the hub"
        )
    }
}

impl std::error::Error for OnlineOnly {}

/// Whether a spend may go ahead. Online, always (the hub decides). Offline, only when the policy
/// allows it; `AllocatedQuota` additionally needs a quota left.
///
/// # Errors
///
/// [`OnlineOnly`] when the spend is refused.
pub fn check_spend(
    policy: BalancePolicy,
    offline: bool,
    quota_left: bool,
) -> Result<(), OnlineOnly> {
    if !offline {
        return Ok(());
    }
    match policy {
        BalancePolicy::OnlineOnly => Err(OnlineOnly),
        BalancePolicy::AllocatedQuota if !quota_left => Err(OnlineOnly),
        BalancePolicy::AllocatedQuota | BalancePolicy::AcceptAndReconcile => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_default_and_the_unknown_are_online_only() {
        assert_eq!(BalancePolicy::default(), BalancePolicy::OnlineOnly);
        assert_eq!(BalancePolicy::parse("whatever"), BalancePolicy::OnlineOnly);
        assert_eq!(
            BalancePolicy::parse("allocated-quota"),
            BalancePolicy::AllocatedQuota
        );
    }

    #[test]
    fn offline_spends_follow_the_policy() {
        assert!(check_spend(BalancePolicy::OnlineOnly, false, false).is_ok());
        let error = check_spend(BalancePolicy::OnlineOnly, true, true).unwrap_err();
        assert!(error.to_string().starts_with(BALANCE_ONLINE_ONLY));
        assert!(check_spend(BalancePolicy::AllocatedQuota, true, true).is_ok());
        assert!(check_spend(BalancePolicy::AllocatedQuota, true, false).is_err());
        assert!(check_spend(BalancePolicy::AcceptAndReconcile, true, false).is_ok());
    }
}
