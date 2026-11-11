//! [`KeyRelease`]: a group key wrapped to one node, under a lease.

use crate::ids::{GroupName, KeyEpoch, NodeId, TenantId};
use crate::time::MonoTime;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A group data key wrapped to one node, valid until `lease_until`.
///
/// The wrapped bytes are ciphertext: safe to send, store and serialise. The lease ends on the
/// node's monotonic clock; the node discards the key when it passes.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRelease {
    /// The tenant.
    pub tenant: TenantId,
    /// The protection group.
    pub group: GroupName,
    /// The epoch released.
    pub epoch: KeyEpoch,
    /// The node it is addressed to.
    pub node: NodeId,
    /// The group key wrapped under the node's wrapping key.
    pub wrapped_key: Vec<u8>,
    /// When the lease ends, on the node's monotonic clock.
    pub lease_until: MonoTime,
}

impl KeyRelease {
    /// The protection group.
    #[must_use]
    pub fn group(&self) -> &GroupName {
        &self.group
    }

    /// The epoch.
    #[must_use]
    pub fn epoch(&self) -> KeyEpoch {
        self.epoch
    }

    /// The node it is addressed to.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// Whether the lease has passed at `now`.
    #[must_use]
    pub fn is_expired(&self, now: MonoTime) -> bool {
        now.is_at_or_after(self.lease_until)
    }
}

impl fmt::Debug for KeyRelease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyRelease")
            .field("tenant", &self.tenant)
            .field("group", &self.group)
            .field("epoch", &self.epoch)
            .field("node", &self.node)
            .field("wrapped_key", &"[wrapped]")
            .field("lease_until", &self.lease_until)
            .finish()
    }
}
