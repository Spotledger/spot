//! The durability policy of a site and how it changes at runtime.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::error::GateError;

/// What the acknowledgement of a write waits for (ADR-061 section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// The local commit is the durability: no wait.
    Local,
    /// A synchronous standby confirms.
    ReplicaSync,
    /// A tail chunk in the object store confirms.
    RemoteSync,
}

impl Kind {
    /// The text readiness and `sys_journal_admin` use.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::ReplicaSync => "replica-sync",
            Self::RemoteSync => "remote-sync",
        }
    }
}

/// What happens when the journal store cannot be reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnUnreachable {
    /// Acknowledge on the local commit after the grace; `RPO_BREACH` is the shipper's.
    Degrade,
    /// Refuse new writes before the handler runs.
    Halt,
}

/// The policy in force.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// What the acknowledgement waits for.
    pub kind: Kind,
    /// What an unreachable store means.
    pub on_unreachable: OnUnreachable,
    /// Whether a journal is configured at all: without one the gate does nothing.
    pub journal_on: bool,
}

/// A change of policy that was accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyChange {
    /// The policy before.
    pub from: Policy,
    /// The policy after.
    pub to: Policy,
    /// Whether the change lowered the durability (always audited).
    pub downgrade: bool,
}

/// The policy cell: read per request, changed at runtime.
pub struct PolicyCell {
    inner: RwLock<Policy>,
}

fn read(lock: &RwLock<Policy>) -> RwLockReadGuard<'_, Policy> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write(lock: &RwLock<Policy>) -> RwLockWriteGuard<'_, Policy> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl PolicyCell {
    /// A cell holding `policy`.
    #[must_use]
    pub fn new(policy: Policy) -> Self {
        Self {
            inner: RwLock::new(policy),
        }
    }

    /// The policy now; a change takes effect on the next request.
    #[must_use]
    pub fn current(&self) -> Policy {
        *read(&self.inner)
    }

    /// Changes the policy.
    ///
    /// A downgrade (a lower [`Kind`]) is allowed only for the tenant administrator
    /// (`is_tenant_admin`); an upgrade needs the shipper healthy. `has_standby` is whether the
    /// site has a standby (`replica-sync` is refused without one).
    ///
    /// # Errors
    /// [`GateError`]; the policy is unchanged then.
    pub fn change(
        &self,
        to: Policy,
        is_tenant_admin: bool,
        shipper_healthy: bool,
        has_standby: bool,
    ) -> Result<PolicyChange, GateError> {
        if to.kind == Kind::ReplicaSync && !has_standby {
            return Err(GateError::NoStandby);
        }
        let from = self.current();
        let downgrade = to.kind < from.kind;
        if downgrade && !is_tenant_admin {
            return Err(GateError::PermissionDenied);
        }
        if to.kind > from.kind && !shipper_healthy {
            return Err(GateError::ShipperUnhealthy);
        }
        *write(&self.inner) = to;
        Ok(PolicyChange {
            from,
            to,
            downgrade,
        })
    }
}
