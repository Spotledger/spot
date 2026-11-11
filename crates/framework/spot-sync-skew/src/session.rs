//! The session decision (ADR-066 section 5, epochs and skew limit).

use serde::{Deserialize, Serialize};

use crate::error::{
    NODE_TOO_OLD, SESSION_EPOCH_UNVERIFIED_HUB, SESSION_EPOCH_UNVERIFIED_NODE,
    SESSION_MIGRATING_HUB, SESSION_MIGRATING_NODE,
};
use crate::model::{NodeReport, Settings, SoftwareVersion};

/// What the refused side does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Retry {
    /// Wait and try the handshake again later.
    Wait,
    /// Only the minimal upgrade channel (offering the required version) is open.
    UpgradeChannelOnly,
}

/// Whether a session may run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// The session runs.
    Run,
    /// The session is refused with a catalogue code and what to do next.
    Refuse {
        /// A stable catalogue code (`sync_skew.*`).
        reason_key: &'static str,
        /// What the refused side does.
        retry: Retry,
    },
}

/// Releases the node is behind the hub. A release is a minor step inside one major, or a whole
/// major step across majors. Across a major boundary the count is a lower bound that cannot fail
/// open: every major step, plus the `hub.minor` releases the hub's own major has already shipped
/// (the node's unknown remaining minors only add to it). A node level with or ahead of the hub is
/// `0` behind. Patch versions never count.
#[must_use]
pub fn releases_behind(hub: SoftwareVersion, node: SoftwareVersion) -> u32 {
    if node.major > hub.major {
        0
    } else if node.major == hub.major {
        hub.minor.saturating_sub(node.minor)
    } else {
        (hub.major - node.major).saturating_add(hub.minor)
    }
}

/// Decide whether a session between the hub and a node may run.
///
/// Order: a hub mid-migration or on an unverified epoch is refused with `Wait` (the node retries);
/// then a node beyond the allowed skew is `NodeTooOld` and gets only the upgrade channel while it
/// keeps operating offline, even on an unverified epoch (the channel is how it recovers); a node
/// mid-migration waits first, and an unverified epoch is `Wait` only for a node within the skew.
#[must_use]
pub fn session_decision(hub: &NodeReport, node: &NodeReport, settings: Settings) -> Decision {
    let wait = |reason_key| Decision::Refuse {
        reason_key,
        retry: Retry::Wait,
    };
    if hub.mid_migration {
        return wait(SESSION_MIGRATING_HUB);
    }
    if !hub.epoch_verified {
        return wait(SESSION_EPOCH_UNVERIFIED_HUB);
    }
    if node.mid_migration {
        return wait(SESSION_MIGRATING_NODE);
    }
    // A node too old gets the upgrade channel whatever its own epoch state is: that
    // channel is how it recovers, so it must never be told to merely wait.
    if releases_behind(hub.software_version, node.software_version) > settings.allowed_release_skew
    {
        return Decision::Refuse {
            reason_key: NODE_TOO_OLD,
            retry: Retry::UpgradeChannelOnly,
        };
    }
    if !node.epoch_verified {
        return wait(SESSION_EPOCH_UNVERIFIED_NODE);
    }
    Decision::Run
}
