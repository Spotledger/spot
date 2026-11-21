//! The per-tenant durability setting (ADR-061 s4).

use serde::{Deserialize, Serialize};

use crate::error::DurabilityError;

/// What `remote-sync` does when the object store cannot be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnUnreachable {
    /// Commit locally, raise an RPO-breach alarm after a grace and re-ship as soon as possible.
    Degrade,
    /// Refuse new commits until the store returns.
    Halt,
}

/// A durability policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "kebab-case")]
pub enum Policy {
    /// Acknowledge when the local commit is durable.
    Local,
    /// Acknowledge when the synchronous standby has the commit.
    ReplicaSync,
    /// Acknowledge when the entry is durable in the object store.
    RemoteSync {
        /// What to do when the store cannot be reached.
        on_unreachable: OnUnreachable,
    },
}

/// A standing warning the platform shows for a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandingWarning {
    /// The tenant runs `local` with no journal target: there is no off-site history.
    NoOffsiteHistory,
}

/// A tenant's durability setting: the policy and whether a journal target is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantDurability {
    /// The chosen policy.
    pub policy: Policy,
    /// Whether the tenant has a journal target (an object store) configured.
    pub journal_target_configured: bool,
}

impl TenantDurability {
    /// The default of ADR-061 s4: SaaS tenants run `replica-sync`; a standalone tenant with a
    /// journal target runs `remote-sync` with `degrade`; a tenant without one runs `local`.
    pub fn default_for(saas: bool, journal_target_configured: bool) -> Self {
        let policy = if saas {
            Policy::ReplicaSync
        } else if journal_target_configured {
            Policy::RemoteSync {
                on_unreachable: OnUnreachable::Degrade,
            }
        } else {
            Policy::Local
        };
        Self {
            policy,
            journal_target_configured,
        }
    }

    /// Check the setting can be applied: `remote-sync` needs a journal target.
    pub fn validate(&self) -> Result<(), DurabilityError> {
        match self.policy {
            Policy::RemoteSync { .. } if !self.journal_target_configured => Err(
                DurabilityError::ConfigInvalid("remote-sync needs a journal target"),
            ),
            _ => Ok(()),
        }
    }

    /// The standing warning for this setting, if any.
    pub fn standing_warning(&self) -> Option<StandingWarning> {
        match self.policy {
            Policy::Local if !self.journal_target_configured => {
                Some(StandingWarning::NoOffsiteHistory)
            }
            _ => None,
        }
    }
}
