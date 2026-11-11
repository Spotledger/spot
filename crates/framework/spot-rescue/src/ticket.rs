//! The two-person `restore` ticket.
//!
//! ADR-22 puts the last-resort rescue behind review. A rescue restores a snapshot and then
//! replays documents through the old version, so it needs two people to agree, exactly like a
//! destructive migration. The approval API is WP-MIG-04's; this crate depends on the
//! [`RestoreApprovals`] trait and never invents approvals itself. [`RestoreTicket`] is the
//! proof that the approval API issued one, and its constructor refuses anything that is not
//! two distinct named people.
//!
//! The ticket binds the snapshot time `T`: an approval for one rescue cannot be replayed against
//! a different restored point.

use std::collections::BTreeSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::error::{Error, Result};

/// Proof that two people approved a restore to one snapshot time.
///
/// The fields are private; the only ways to obtain a value are [`RestoreTicket::two_person`]
/// and an implementation of [`RestoreApprovals`] (the adapter over WP-MIG-04's approval API).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreTicket {
    id: String,
    snapshot_at: i64,
    approvers: Vec<String>,
    issued_at: i64,
}

impl RestoreTicket {
    /// A ticket from two distinct, non-empty approver names.
    ///
    /// # Errors
    /// [`Error::NotTwoPerson`] when fewer than two distinct names are given or a name is empty,
    /// and [`Error::InvalidInput`] when the ticket id is empty.
    pub fn two_person(
        id: impl Into<String>,
        snapshot_at: i64,
        first: impl Into<String>,
        second: impl Into<String>,
        issued_at: i64,
    ) -> Result<Self> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(Error::InvalidInput {
                what: "restore ticket id".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        let mut set = BTreeSet::new();
        for name in [first.into(), second.into()] {
            let name = name.trim().to_owned();
            if !name.is_empty() {
                set.insert(name);
            }
        }
        let approvers: Vec<String> = set.into_iter().collect();
        if approvers.len() < 2 {
            return Err(Error::NotTwoPerson {
                approvers: approvers.len(),
            });
        }
        Ok(Self {
            id,
            snapshot_at,
            approvers,
            issued_at,
        })
    }

    /// The approval record's id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The snapshot time this ticket authorises.
    #[must_use]
    pub fn snapshot_at(&self) -> i64 {
        self.snapshot_at
    }

    /// The distinct approvers, in name order.
    #[must_use]
    pub fn approvers(&self) -> &[String] {
        &self.approvers
    }

    /// When the approval API issued the ticket (unix microseconds).
    #[must_use]
    pub fn issued_at(&self) -> i64 {
        self.issued_at
    }

    /// Whether the ticket is for `snapshot_at` and names two distinct people.
    #[must_use]
    pub fn authorises(&self, snapshot_at: i64) -> bool {
        self.snapshot_at == snapshot_at && self.approvers.len() >= 2
    }
}

/// The approval API seam (WP-MIG-04): issues the two-person `restore` ticket for a snapshot
/// time, or `None` when the site's policy has not approved one.
///
/// The real implementation wraps the migration approval API; [`MemoryRestoreApprovals`] is the
/// reference double a host uses before that wiring exists.
pub trait RestoreApprovals: Send + Sync {
    /// The ticket that authorises restoring the snapshot taken at `snapshot_at`, if any.
    ///
    /// # Errors
    /// A backend error when the approval store cannot be read.
    fn restore_ticket(
        &self,
        snapshot_at: i64,
        now: i64,
    ) -> BoxFuture<'_, Result<Option<RestoreTicket>>>;
}

/// An approval source holding at most one already-issued ticket.
///
/// It is useful for a host that obtained the ticket elsewhere and for tests; it never invents
/// approvals, it only returns the value it was built with.
#[derive(Debug)]
pub struct MemoryRestoreApprovals {
    ticket: Mutex<Option<RestoreTicket>>,
}

impl MemoryRestoreApprovals {
    /// A source with no ticket.
    #[must_use]
    pub fn none() -> Self {
        Self {
            ticket: Mutex::new(None),
        }
    }

    /// A source that returns `ticket`.
    #[must_use]
    pub fn with_ticket(ticket: RestoreTicket) -> Self {
        Self {
            ticket: Mutex::new(Some(ticket)),
        }
    }
}

impl Default for MemoryRestoreApprovals {
    fn default() -> Self {
        Self::none()
    }
}

impl RestoreApprovals for MemoryRestoreApprovals {
    fn restore_ticket(
        &self,
        snapshot_at: i64,
        _now: i64,
    ) -> BoxFuture<'_, Result<Option<RestoreTicket>>> {
        Box::pin(async move {
            let ticket = self
                .ticket
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            Ok(ticket.filter(|ticket| ticket.authorises(snapshot_at)))
        })
    }
}
