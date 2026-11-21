//! The signed permission snapshot (ADR-070 rule 12.13 b): what a node is allowed to believe
//! while it cannot reach the hub.
//!
//! The hub publishes one per node: tenant, node, a monotonic epoch, `issued_at`, a
//! `valid_until` per tier, the compiled grants, the user directory with `enabled` flags and
//! the per-user offline allowlist, the sync-class table, the key epoch and the revocations.
//! [`Snapshot::canonical_bytes`] is the one byte form the hash and the signature cover; the
//! hash is what tells a fork (same epoch, different content) from a replay.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::canonical::Enc;
use crate::error::Rejection;
use crate::types::{HubMs, NodeId, TenantId, Tier};

/// The domain tag of a snapshot's signed bytes.
const SNAPSHOT_DOMAIN: &str = "SPOT-OFFLINE-SNAPSHOT-V1";

/// A SHA-256 over a snapshot's canonical bytes.
pub type SnapshotHash = [u8; 32];

/// Per-tier deadlines in hub time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierDeadlines(pub [HubMs; 3]);

impl TierDeadlines {
    /// The deadline of `tier`.
    #[must_use]
    pub fn get(&self, tier: Tier) -> HubMs {
        self.0[tier.index()]
    }
}

/// One compiled grant: a permission a user holds through one assignment. The hub compiles
/// these from the `spot-iam-roles` snapshot input (a role's flattened rights per active
/// assignment), so the node never recompiles roles.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The user.
    pub user: String,
    /// The assignment that gives the rights; scope stays bound to it.
    pub assignment_id: String,
    /// The role of the assignment, for display and audit.
    pub role: String,
    /// The permissions, as `doctype:right` or a platform permission such as
    /// `sync.override_stale`.
    pub permissions: Vec<String>,
}

/// One user of the directory the node holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    /// The user.
    pub user: String,
    /// The hub's `enabled` flag at issue time.
    pub enabled: bool,
    /// Whether the user may work on this node at all while it is offline. Administrators are
    /// not allowed by default (rule 12.13 g).
    pub offline_allowed: bool,
    /// The most sensitive tier the user may act at offline. Their sign-in needs this tier's
    /// lease, so a user with payment roles loses offline access on the short bound.
    pub highest_tier: Tier,
}

/// How a protection group syncs (rule 12.13 a).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncClass {
    /// Syncs like any field.
    Everywhere,
    /// Stored encrypted; the key is released under a lease.
    Encrypted,
    /// The node can write but never read back.
    Sealed,
    /// Does not sync to branch nodes.
    HomeOnly,
}

impl SyncClass {
    fn tag(self) -> u8 {
        match self {
            Self::Everywhere => 1,
            Self::Encrypted => 2,
            Self::Sealed => 3,
            Self::HomeOnly => 4,
        }
    }
}

/// A protection group and its class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncClassEntry {
    /// The protection group.
    pub group: String,
    /// Its class.
    pub class: SyncClass,
}

/// Whether a node is suspended (may return) or decommissioned (gone for good).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeRevocationKind {
    /// Connections and key release are refused until the node is reinstated.
    Suspended,
    /// The node is retired; it never returns.
    Decommissioned,
}

/// A revocation entry carried in the next snapshot (rule 12.13 h).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Revocation {
    /// A node was suspended or decommissioned.
    Node {
        /// The node.
        node: String,
        /// Which.
        kind: NodeRevocationKind,
        /// The first snapshot epoch that carried it.
        effective_epoch: u64,
    },
    /// A user (every assignment) or one assignment was revoked.
    User {
        /// The user.
        user: String,
        /// `None` revokes the user entirely; `Some` revokes that assignment only.
        assignment_id: Option<String>,
        /// The first snapshot epoch that carried it.
        effective_epoch: u64,
    },
}

/// The signed content (rule 12.13 b).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The tenant.
    pub tenant: TenantId,
    /// The node it is for.
    pub node: NodeId,
    /// The monotonic epoch. A node never accepts a lower one.
    pub epoch: u64,
    /// Hub time of issue.
    pub issued_at: HubMs,
    /// Hub-time deadline per tier. The lease of a tier is its deadline minus the issue time.
    pub valid_until: TierDeadlines,
    /// The compiled grants.
    pub grants: Vec<Grant>,
    /// The user directory.
    pub directory: Vec<DirectoryEntry>,
    /// The sync-class table.
    pub sync_classes: Vec<SyncClassEntry>,
    /// The key epoch current at issue (SEC-10).
    pub key_epoch: u64,
    /// The revocations known at issue.
    pub revocations: Vec<Revocation>,
}

impl Snapshot {
    /// The canonical bytes: lists are sorted, so the same content has one byte form whatever
    /// order the hub built it in.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new(SNAPSHOT_DOMAIN);
        e.str(self.tenant.as_str())
            .str(self.node.as_str())
            .u64(self.epoch)
            .u64(self.issued_at);
        for t in Tier::ALL {
            e.u64(self.valid_until.get(t));
        }
        let mut grants: Vec<&Grant> = self.grants.iter().collect();
        grants.sort_by(|a, b| (&a.user, &a.assignment_id).cmp(&(&b.user, &b.assignment_id)));
        e.len(grants.len());
        for g in grants {
            let mut perms: Vec<&String> = g.permissions.iter().collect();
            perms.sort();
            e.str(&g.user)
                .str(&g.assignment_id)
                .str(&g.role)
                .len(perms.len());
            for p in perms {
                e.str(p);
            }
        }
        let mut dir: Vec<&DirectoryEntry> = self.directory.iter().collect();
        dir.sort_by(|a, b| a.user.cmp(&b.user));
        e.len(dir.len());
        for d in dir {
            e.str(&d.user)
                .bool(d.enabled)
                .bool(d.offline_allowed)
                .u8(u8::try_from(d.highest_tier.index()).unwrap_or(u8::MAX));
        }
        let mut classes: Vec<&SyncClassEntry> = self.sync_classes.iter().collect();
        classes.sort_by(|a, b| a.group.cmp(&b.group));
        e.len(classes.len());
        for c in classes {
            e.str(&c.group).u8(c.class.tag());
        }
        e.u64(self.key_epoch);
        let mut revs: Vec<Vec<u8>> = self.revocations.iter().map(revocation_bytes).collect();
        revs.sort();
        e.len(revs.len());
        for r in &revs {
            e.bytes(r);
        }
        e.finish()
    }

    /// The hash of the canonical bytes.
    #[must_use]
    pub fn hash(&self) -> SnapshotHash {
        Sha256::digest(self.canonical_bytes()).into()
    }

    /// Checks the structure: no duplicate user, assignment or group, no empty names. A
    /// duplicate directory entry is refused because two entries for one user could disagree
    /// on `enabled`, and the node must never pick the friendlier one.
    ///
    /// # Errors
    /// [`Rejection::Malformed`] naming the first problem found.
    pub fn validate(&self) -> Result<(), Rejection> {
        let mut users = BTreeSet::new();
        for d in &self.directory {
            if d.user.is_empty() || !users.insert(d.user.as_str()) {
                return Err(malformed("the directory has an empty or duplicate user"));
            }
        }
        let mut assignments = BTreeSet::new();
        for g in &self.grants {
            if g.user.is_empty()
                || g.assignment_id.is_empty()
                || !assignments.insert((g.user.as_str(), g.assignment_id.as_str()))
            {
                return Err(malformed("a grant has an empty or duplicate assignment"));
            }
        }
        let mut groups = BTreeSet::new();
        for c in &self.sync_classes {
            if c.group.is_empty() || !groups.insert(c.group.as_str()) {
                return Err(malformed(
                    "the sync classes have an empty or duplicate group",
                ));
            }
        }
        if self.epoch == 0 {
            return Err(malformed("epoch zero is the absent epoch"));
        }
        Ok(())
    }
}

fn malformed(detail: &'static str) -> Rejection {
    Rejection::Malformed { detail }
}

fn revocation_bytes(r: &Revocation) -> Vec<u8> {
    let mut e = Enc::new("rev");
    match r {
        Revocation::Node {
            node,
            kind,
            effective_epoch,
        } => {
            e.u8(1).str(node).u8(match kind {
                NodeRevocationKind::Suspended => 1,
                NodeRevocationKind::Decommissioned => 2,
            });
            e.u64(*effective_epoch);
        }
        Revocation::User {
            user,
            assignment_id,
            effective_epoch,
        } => {
            e.u8(2).str(user);
            match assignment_id {
                None => e.u8(0),
                Some(a) => e.u8(1).str(a),
            };
            e.u64(*effective_epoch);
        }
    }
    e.finish()
}
