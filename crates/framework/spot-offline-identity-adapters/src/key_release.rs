//! The key-release adapter: the node's key comes from the key hierarchy, wrapped, or not at
//! all.
//!
//! [`ProtectedKeysRelease`] implements the core crate's
//! [`KeyReleasePort`](spot_offline_identity::KeyReleasePort) over
//! `spot_protected_keys::Hub::release` (WP-SEC-10.T1). On the hub, the gate of the core crate
//! ([`GatedKeyRelease`](spot_offline_identity::GatedKeyRelease)) sits in front of this adapter;
//! this adapter re-checks the two facts the port itself carries, so it is safe to call on its
//! own too:
//!
//! - the request's tenant must be the hub's tenant;
//! - the node the payload claims must be the node the connection authenticated as.
//!
//! What comes back is the group key **wrapped to the node's wrapping key**: ciphertext that is
//! safe to send and store. Every refusal of the hierarchy is mapped to
//! [`OfflineError::KeyReleaseRefused`] with a fixed reason, so an error never carries a wrapped
//! key, key material or the hierarchy's own message.
//!
//! Time is the hub's monotonic reading, passed in: this crate never reads a clock.

use spot_offline_identity::{KeyReleasePort, KeyReleaseRequest, OfflineError};
use spot_protected_keys::error::ProtectedKeyError;
use spot_protected_keys::time::MonoTime;
use spot_protected_keys::{GroupName, Hub, NodeId as KeyNodeId, PermissionTier, RootKeyProvider};

/// A [`KeyReleasePort`] over `spot_protected_keys::Hub::release`.
///
/// The hub's monotonic reading is held here because the port carries no time; advance it with
/// [`set_now`](Self::set_now) as the hub's clock moves. A release after the lease has passed is
/// refused by the hierarchy itself.
pub struct ProtectedKeysRelease<'a, R: RootKeyProvider, T: PermissionTier> {
    hub: &'a mut Hub<R>,
    tiers: &'a T,
    now: MonoTime,
}

impl<'a, R: RootKeyProvider, T: PermissionTier> ProtectedKeysRelease<'a, R, T> {
    /// The adapter over `hub`, asking `tiers` for the node's tier, at the hub clock `now`.
    pub fn new(hub: &'a mut Hub<R>, tiers: &'a T, now: MonoTime) -> Self {
        Self { hub, tiers, now }
    }

    /// The hub clock the next release is judged at.
    pub fn set_now(&mut self, now: MonoTime) {
        self.now = now;
    }

    /// The hub clock the next release is judged at.
    #[must_use]
    pub fn now(&self) -> MonoTime {
        self.now
    }

    fn refuse(request: &KeyReleaseRequest, reason: &'static str) -> OfflineError {
        OfflineError::KeyReleaseRefused {
            node: request.claimed_node.as_str().to_owned(),
            reason,
        }
    }
}

/// A fixed, key-free reason for a hierarchy refusal. The message of the hierarchy error names
/// ids only, but the port carries a `&'static str`, so each code gets its own plain reason.
fn reason_for(error: &ProtectedKeyError) -> &'static str {
    match error.code() {
        spot_protected_keys::error::UNKNOWN_GROUP => "the protection group has no key",
        spot_protected_keys::error::UNKNOWN_EPOCH => {
            "the protection group has no key at that epoch"
        }
        spot_protected_keys::error::EPOCH_RETIRED => "the key of that epoch is retired",
        spot_protected_keys::error::UNKNOWN_NODE => "the node is not registered",
        spot_protected_keys::error::NODE_NOT_ACTIVE => "the node is suspended",
        spot_protected_keys::error::NODE_REVOKED => "the node is revoked",
        spot_protected_keys::error::NO_PERMISSION => {
            "the node has no tier for that protection group"
        }
        spot_protected_keys::error::WRONG_NODE => "the key is addressed to another node",
        spot_protected_keys::error::LEASE_EXPIRED => "the key lease has passed",
        spot_protected_keys::error::TAMPERED => "a key or ciphertext failed authentication",
        spot_protected_keys::error::MALFORMED => "the key hierarchy refused a malformed request",
        spot_protected_keys::error::ROOT_KEY => "the root key provider failed",
        spot_protected_keys::error::BAD_ID => "the node or group id is not valid",
        _ => "the key release was refused",
    }
}

impl<R: RootKeyProvider, T: PermissionTier> KeyReleasePort for ProtectedKeysRelease<'_, R, T> {
    fn release(&mut self, request: &KeyReleaseRequest) -> Result<Vec<u8>, OfflineError> {
        if request.tenant.as_str() != self.hub.tenant().as_str() {
            return Err(Self::refuse(request, "another tenant"));
        }
        if request.claimed_node != request.authenticated_node {
            return Err(Self::refuse(
                request,
                "the node is not the one that authenticated",
            ));
        }
        let node = KeyNodeId::new(request.authenticated_node.as_str())
            .map_err(|_| Self::refuse(request, "the node id is not valid"))?;
        let group = GroupName::new(&request.group)
            .map_err(|_| Self::refuse(request, "the group name is not valid"))?;
        let release = self
            .hub
            .release(&node, &group, self.now, self.tiers)
            .map_err(|e| Self::refuse(request, reason_for(&e)))?;
        // Only the wrapped ciphertext leaves this adapter. The lease and the epoch stay with
        // the caller's record of the release; nothing else is returned.
        Ok(release.wrapped_key)
    }
}
