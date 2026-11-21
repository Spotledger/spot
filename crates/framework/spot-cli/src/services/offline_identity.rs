//! Offline identity on the hub (WP-IAM-09.T4). Operator page: `docs/running-spot/offline_identity.md`.
//!
//! `spot-offline-identity` owns the signed snapshot, lease and checks; the sync modules own the carrier and
//! the keys. This module decides **what goes into** a snapshot and what the hub believes afterwards, from the
//! hub's own role snapshot and directory, never from what a node claims:
//!
//! - [`OfflineIdentity::issue`]: the assignments in force now with their flattened permissions and own
//!   scope, the directory, tier bounds, an expiry from `identity_lease_hours` and an epoch that only rises.
//!   The same content keeps its epoch; any change raises it and a removed assignment or user becomes a
//!   revocation (kept until reinstated). Without a role snapshot or directory nothing is issued.
//! - [`OfflineIdentity::stale_nodes`], [`OfflineIdentity::needs_you`]: which nodes hold an old epoch.
//! - [`OfflineIdentity::sync`]: offline writes sorted by the hub's **current** revocation state through
//!   `sync_journal`; a revoked author's write is quarantined (never dropped) through the caller's queue.
//! - [`ENTRY_POINTS`], [`decide_at`]: every way a request reaches data on a node, one decision.
//!
//! Level: optional; a missing port or a stale node degrades the site and nothing widens access.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use spot_audit_chain::{ChainEntry, Hash, genesis_hash};
use spot_offline_identity::Persistence;
use spot_offline_identity::signing::NodeReport;
use spot_offline_identity::{
    AuditSink, Decision, DirectoryEntry, Grant, HubView, NodeClock, NodeId, NodeState,
    QuarantinePort, Revocation, Snapshot, SnapshotStore, SyncClassEntry, SyncReport, TenantId,
    Tier, TierBounds, TierDeadlines, clamp, sync_journal,
};
use spot_permissions::UserPermission;

use crate::services::roles::{Published, RoleSnapshots};
use crate::services::{CheckFuture, Idle, Level, SiteCtx};
use crate::{Error, Result};

/// Start order: after the roles (1045) and edge offline (1210) modules.
pub const ORDER: u32 = 1230;

/// The module name, also the settings table.
pub const NAME: &str = "offline_identity";

/// Readiness code: no published role snapshot.
pub const CODE_NO_ROLES: &str = "offline_identity_host.no_roles";
/// Readiness code: no directory port.
pub const CODE_NO_DIRECTORY: &str = "offline_identity_host.no_directory";
/// Readiness code: a node holds an old epoch.
pub const CODE_STALE_NODES: &str = "offline_identity_host.stale_nodes";

const HOUR_MS: u64 = 3_600_000;
/// The most users one snapshot may carry (bounded input).
const MAX_USERS: usize = 10_000;
/// The most revocations the hub keeps and signs into a snapshot.
const MAX_REVOCATIONS: usize = 4_096;
/// A snapshot is re-issued when less than 1/this of its shortest lease is left.
const REFRESH_DIVISOR: u64 = 2;

/// `[services.offline_identity]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site issues offline identity at all.
    pub enabled: bool,
    /// The identity lease: how long a snapshot lets a node confirm a user, hours (1 to 168).
    pub identity_lease_hours: u64,
    /// The age bounds of tier 1, 2 and 3 writes, minutes (1 minute to 7 days each).
    pub tier_bounds_minutes: [u64; 3],
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            identity_lease_hours: 24,
            tier_bounds_minutes: [60, 720, 1_440],
        }
    }
}

impl Settings {
    /// The first key out of range, or `None`.
    #[must_use]
    pub fn invalid_key(&self) -> Option<&'static str> {
        if !(1..=168).contains(&self.identity_lease_hours) {
            return Some("identity_lease_hours");
        }
        if !self.bounds().in_range() || self.tier_bounds_minutes.contains(&0) {
            return Some("tier_bounds_minutes");
        }
        None
    }

    /// The tier bounds as the classifier takes them.
    pub fn bounds(&self) -> TierBounds {
        let [one, two, three] = self.tier_bounds_minutes;
        let ms = |minutes: u64| minutes.saturating_mul(60_000);
        TierBounds::clamped(ms(one), ms(two), ms(three))
    }

    /// The identity lease in milliseconds.
    pub fn lease_ms(&self) -> u64 {
        clamp(self.identity_lease_hours.saturating_mul(HOUR_MS))
    }
}

/// One user the hub lets a node serve: a row of the snapshot's directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServedUser {
    /// The user.
    pub user: String,
    /// Whether the account is enabled.
    pub enabled: bool,
    /// Whether the user may work offline at all.
    pub offline_allowed: bool,
    /// The most sensitive tier the user may act at offline.
    pub highest_tier: Tier,
}

/// What the hub knows beyond the role snapshot (node registry, user directory): another module provides it.
pub trait HubFacts: Send + Sync {
    /// The users `node` serves; an error refuses the issue.
    ///
    /// # Errors
    /// The directory could not be read.
    fn served_users(&self, node: &NodeId) -> std::result::Result<Vec<ServedUser>, String>;
    /// The registry's state of `node`, or `None` when it is not registered.
    fn node_state(&self, node: &NodeId) -> Option<NodeState>;
    /// The protected key epoch to put in the snapshot.
    fn key_epoch(&self) -> u64;
    /// The sync classes a node must honour.
    fn sync_classes(&self) -> Vec<SyncClassEntry>;
}

/// Why nothing was issued or sorted (no value, no secret).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueError {
    /// The published role snapshot is not available (fail closed).
    NoRoleSnapshot,
    /// The directory could not be read.
    DirectoryUnavailable,
    /// The node is unknown, suspended or decommissioned: it gets no snapshot.
    NodeNotActive,
    /// More revocations than the hub keeps; an operator has to prune.
    TooManyRevocations,
    /// A node reported an epoch the hub never issued.
    EpochNeverIssued,
}

/// A node that holds an older epoch than the hub's latest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleNode {
    /// The node.
    pub node: NodeId,
    /// The epoch it reported.
    pub held: u64,
    /// The latest epoch issued.
    pub latest: u64,
}

/// The permissions an assignment carries, sorted: `doctype.right[:owner]`, the app permission names and
/// `scope:<doctype|*>:<field>=<allowed|...>` per restriction of its own scope (field grants stay on the node).
#[must_use]
pub fn permission_strings(
    published: &Published,
    roles: &[String],
    scope: &[UserPermission],
) -> Vec<String> {
    let mut out = BTreeSet::new();
    for role in roles {
        let Some(resolved) = published
            .input
            .resolved
            .get(&spot_iam_roles::RoleName::from(role.as_str()))
        else {
            continue;
        };
        for grant in &resolved.grants {
            for right in grant.grant.rights.as_slice() {
                let Some(text) = serde_json::to_value(right)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                else {
                    continue;
                };
                let owner = if grant.grant.if_owner { ":owner" } else { "" };
                out.insert(format!("{}.{text}{owner}", grant.grant.doctype));
            }
        }
        out.extend(
            resolved
                .app_permissions
                .iter()
                .map(|p| p.permission.as_str().to_owned()),
        );
    }
    for restriction in scope {
        let mut allowed = restriction.allowed.clone();
        allowed.sort();
        out.insert(format!(
            "scope:{}:{}={}",
            restriction.doctype.as_deref().unwrap_or("*"),
            restriction.field_or_link_doctype,
            allowed.join("|")
        ));
    }
    out.into_iter().collect()
}

/// The grants and directory of one node from the published role snapshot and the served users.
/// A user that is disabled or not allowed offline appears in the directory with no grants.
fn content(published: &Published, served: &[ServedUser]) -> (Vec<Grant>, Vec<DirectoryEntry>) {
    let mut grants = Vec::new();
    let mut directory = Vec::new();
    for user in served {
        directory.push(DirectoryEntry {
            user: user.user.clone(),
            enabled: user.enabled,
            offline_allowed: user.offline_allowed,
            highest_tier: user.highest_tier,
        });
        if !user.enabled || !user.offline_allowed {
            continue;
        }
        for assignment in published
            .input
            .assignments
            .get(&user.user)
            .into_iter()
            .flatten()
        {
            let mut roles: Vec<String> = assignment.roles.iter().map(|r| r.0.clone()).collect();
            roles.sort();
            grants.push(Grant {
                user: user.user.clone(),
                assignment_id: assignment.id.clone(),
                role: roles.join("+"),
                permissions: permission_strings(published, &roles, &assignment.scope),
            });
        }
    }
    (grants, directory)
}

/// The hash of the snapshot without what changes without a change of meaning: decides a new epoch.
fn fingerprint(snapshot: &Snapshot) -> [u8; 32] {
    let mut neutral = snapshot.clone();
    neutral.epoch = 1;
    neutral.issued_at = 0;
    neutral.valid_until = TierDeadlines([0, 0, 0]);
    Sha256::digest(neutral.canonical_bytes()).into()
}

#[derive(Default)]
struct NodeLedger {
    issued: u64,
    held: u64,
    last: Option<Snapshot>,
    fingerprint: Option<[u8; 32]>,
    bounds: BTreeMap<u64, TierBounds>,
    device_seq: u64,
    journal_head: Option<(u64, Hash)>,
}

#[derive(Default)]
struct Inner {
    nodes: BTreeMap<NodeId, NodeLedger>,
    revocations: Vec<Revocation>,
    known: BTreeSet<(String, String)>,
}

/// The hub side of offline identity for one tenant.
pub struct OfflineIdentity {
    tenant: TenantId,
    settings: Settings,
    roles: Arc<dyn RoleSnapshots>,
    facts: Arc<dyn HubFacts>,
    inner: Mutex<Inner>,
}

impl OfflineIdentity {
    /// A hub over the role snapshot and the hub's facts.
    #[must_use]
    pub fn new(
        tenant: TenantId,
        settings: Settings,
        roles: Arc<dyn RoleSnapshots>,
        facts: Arc<dyn HubFacts>,
    ) -> Self {
        Self {
            tenant,
            settings,
            roles,
            facts,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The unsigned snapshot for `node` at the hub's time: the same content keeps its epoch while more
    /// than half of its lease is left; a change, or an old snapshot, gets the next epoch.
    ///
    /// # Errors
    /// [`IssueError`]; nothing is recorded when it fails.
    pub fn issue(
        &self,
        node: &NodeId,
        hub_now_ms: u64,
    ) -> std::result::Result<Snapshot, IssueError> {
        if self.facts.node_state(node) != Some(NodeState::Active) {
            return Err(IssueError::NodeNotActive);
        }
        let published = self.roles.current().ok_or(IssueError::NoRoleSnapshot)?;
        let served = self
            .facts
            .served_users(node)
            .map_err(|_| IssueError::DirectoryUnavailable)?;
        if served.len() > MAX_USERS {
            return Err(IssueError::DirectoryUnavailable);
        }
        let (grants, directory) = content(&published, &served);
        let mut inner = self.lock();
        let ledger_epoch = inner.nodes.get(node).map_or(0, |l| l.issued);
        let epoch = ledger_epoch.saturating_add(1);
        let previous = inner.nodes.get(node).and_then(|l| l.last.clone());

        // Revocations: an assignment granted before and not now, and a user no longer served or
        // enabled. A reinstated user's revocations end.
        let mut revocations = inner.revocations.clone();
        let granted: BTreeSet<(&str, &str)> = grants
            .iter()
            .map(|g| (g.user.as_str(), g.assignment_id.as_str()))
            .collect();
        let serving: BTreeSet<&str> = directory
            .iter()
            .filter(|d| d.enabled && d.offline_allowed)
            .map(|d| d.user.as_str())
            .collect();
        if let Some(previous) = &previous {
            let mut revoke = |user: &str, assignment: Option<&str>| {
                let revoked = Revocation::User {
                    user: user.to_owned(),
                    assignment_id: assignment.map(str::to_owned),
                    effective_epoch: epoch,
                };
                if !revocations.iter().any(|r| same_target(r, &revoked)) {
                    revocations.push(revoked);
                }
            };
            for old in &previous.grants {
                if !granted.contains(&(old.user.as_str(), old.assignment_id.as_str())) {
                    revoke(&old.user, Some(&old.assignment_id));
                }
            }
            for old in &previous.directory {
                if old.enabled && old.offline_allowed && !serving.contains(old.user.as_str()) {
                    revoke(&old.user, None);
                }
            }
        }
        revocations.retain(|r| match r {
            Revocation::User {
                user,
                assignment_id: Some(a),
                ..
            } => !granted.contains(&(user.as_str(), a.as_str())),
            Revocation::User {
                user,
                assignment_id: None,
                ..
            } => !serving.contains(user.as_str()),
            Revocation::Node { .. } => true,
        });
        if revocations.len() > MAX_REVOCATIONS {
            return Err(IssueError::TooManyRevocations);
        }

        let bounds = self.settings.bounds();
        let lease = self.settings.lease_ms();
        let deadline = |tier: Tier| hub_now_ms.saturating_add(lease.min(bounds.get(tier)));
        let snapshot = Snapshot {
            tenant: self.tenant.clone(),
            node: node.clone(),
            epoch,
            issued_at: hub_now_ms,
            valid_until: TierDeadlines([
                deadline(Tier::One),
                deadline(Tier::Two),
                deadline(Tier::Three),
            ]),
            grants,
            directory,
            sync_classes: self.facts.sync_classes(),
            key_epoch: self.facts.key_epoch(),
            revocations,
        };
        let print = fingerprint(&snapshot);
        if let Some(previous) = previous {
            let unchanged = inner.nodes.get(node).and_then(|l| l.fingerprint) == Some(print);
            let shortest = previous
                .valid_until
                .get(Tier::One)
                .saturating_sub(previous.issued_at);
            let left = previous
                .valid_until
                .get(Tier::One)
                .saturating_sub(hub_now_ms);
            if unchanged && left > shortest / REFRESH_DIVISOR {
                return Ok(previous);
            }
        }
        snapshot
            .validate()
            .map_err(|_| IssueError::DirectoryUnavailable)?;
        inner.known.extend(
            snapshot
                .grants
                .iter()
                .map(|g| (g.user.clone(), g.assignment_id.clone())),
        );
        inner.revocations.clone_from(&snapshot.revocations);
        let ledger = inner.nodes.entry(node.clone()).or_default();
        ledger.issued = epoch;
        ledger.bounds.insert(epoch, bounds);
        ledger.fingerprint = Some(print);
        ledger.last = Some(snapshot.clone());
        Ok(snapshot)
    }

    /// Records the epoch a node says it holds.
    ///
    /// # Errors
    /// [`IssueError::EpochNeverIssued`] for an epoch above the latest issued to it.
    pub fn record_report(
        &self,
        node: &NodeId,
        report: &NodeReport,
    ) -> std::result::Result<(), IssueError> {
        let mut inner = self.lock();
        let ledger = inner.nodes.entry(node.clone()).or_default();
        if report.highest_epoch > ledger.issued {
            return Err(IssueError::EpochNeverIssued);
        }
        ledger.held = ledger.held.max(report.highest_epoch);
        Ok(())
    }

    /// The nodes that hold an older epoch than the latest issued to them.
    #[must_use]
    pub fn stale_nodes(&self) -> Vec<StaleNode> {
        self.lock()
            .nodes
            .iter()
            .filter(|(_, l)| l.issued > 0 && l.held < l.issued)
            .map(|(node, l)| StaleNode {
                node: node.clone(),
                held: l.held,
                latest: l.issued,
            })
            .collect()
    }

    /// The "needs you" lines: one sentence for each stale node (no value, no secret).
    #[must_use]
    pub fn needs_you(&self) -> Vec<String> {
        let line = |s: StaleNode| {
            format!(
                "offline_identity.stale_snapshot: node {} holds permission epoch {} and the latest is {}",
                s.node, s.held, s.latest
            )
        };
        self.stale_nodes().into_iter().map(line).collect()
    }

    /// Sorts the offline journal entries of `node` against the hub's **current** state: unknown epoch,
    /// forged assignment, revoked author, bound exceeded and so on are quarantined (never dropped)
    /// through `queue`. The hub's position in the journal advances only past what was placed.
    ///
    /// # Errors
    /// [`IssueError::NodeNotActive`] when the node is not registered.
    pub fn sync<Q: QuarantinePort, A: AuditSink>(
        &self,
        node: &NodeId,
        entries: &[ChainEntry],
        queue: &mut Q,
        audit: &mut A,
        at_micros: i64,
    ) -> std::result::Result<SyncReport, IssueError> {
        let state = self
            .facts
            .node_state(node)
            .ok_or(IssueError::NodeNotActive)?;
        let live: BTreeSet<(String, String)> = self
            .roles
            .current()
            .map(|p| {
                p.input
                    .assignments
                    .iter()
                    .flat_map(|(user, list)| list.iter().map(move |a| (user.clone(), a.id.clone())))
                    .collect()
            })
            .unwrap_or_default();
        let mut inner = self.lock();
        let mut assignments = inner.known.clone();
        assignments.extend(live);
        let revocations = inner.revocations.clone();
        let ledger = inner.nodes.entry(node.clone()).or_default();
        let view = HubView {
            tenant: &self.tenant,
            sender: node,
            sender_state: state,
            latest_epoch: ledger.issued,
            bounds_by_epoch: &ledger.bounds,
            revocations: &revocations,
            assignments: &assignments,
            last_device_seq: ledger.device_seq,
        };
        let (prev_seq, prev_hash) = ledger
            .journal_head
            .unwrap_or((0, genesis_hash(node.as_str())));
        // Entries the hub already placed (a resend after a lost acknowledgement) are not sorted again.
        let fresh: Vec<ChainEntry> = entries
            .iter()
            .filter(|e| e.seq > prev_seq)
            .cloned()
            .collect();
        let report = sync_journal(&view, prev_seq, prev_hash, &fresh, queue, audit, at_micros);
        if let Some(highest) = report.highest_device_seq {
            ledger.device_seq = ledger.device_seq.max(highest);
        }
        if let Some(entry) = entries.iter().find(|e| e.seq == report.ack_through) {
            ledger.journal_head = Some((entry.seq, entry.hash));
        }
        Ok(report)
    }
}

fn same_target(a: &Revocation, b: &Revocation) -> bool {
    match (a, b) {
        (
            Revocation::User {
                user: u1,
                assignment_id: a1,
                ..
            },
            Revocation::User {
                user: u2,
                assignment_id: a2,
                ..
            },
        ) => u1 == u2 && a1 == a2,
        (
            Revocation::Node {
                node: n1, kind: k1, ..
            },
            Revocation::Node {
                node: n2, kind: k2, ..
            },
        ) => n1 == n2 && k1 == k2,
        _ => false,
    }
}

/// Every way a request reaches data on a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryPoint {
    /// The HTTP API (resource, list, report, export routes).
    Api,
    /// Realtime subscriptions and pushes.
    Realtime,
    /// Background jobs and the scheduler (they act as a user or as the system on a user's behalf).
    Jobs,
    /// Server Scripts.
    Scripts,
    /// App controllers (native and TypeScript) and their whitelisted methods.
    Controllers,
    /// The management API and the `spot` commands that touch data.
    Management,
}

impl EntryPoint {
    /// Every entry point. A new variant must be added here and to [`ENTRY_POINTS`]; a test refuses a
    /// table that does not list exactly these.
    pub const ALL: [EntryPoint; 6] = [
        Self::Api,
        Self::Realtime,
        Self::Jobs,
        Self::Scripts,
        Self::Controllers,
        Self::Management,
    ];
}

/// One row of the coverage table: the entry point and where its offline decision is taken.
#[derive(Clone, Copy, Debug)]
pub struct EntryRow {
    /// The entry point.
    pub point: EntryPoint,
    /// The file whose code must call [`decide_at`] for it.
    pub enforced_in: &'static str,
    /// What is decided and for whom.
    pub rule: &'static str,
    /// Whether the file already calls [`decide_at`]. Every row is `false` in this task: the files are
    /// owned by other tasks (see the operator page); a row flips to `true` in the PR that wires it.
    pub wired: bool,
}

/// The coverage table. Offline mode is where a permission check is easiest to forget, so the table is
/// part of the deliverable: every row names the file that must call [`decide_at`], the rule, and whether
/// it does yet (`false` everywhere in this task: those files belong to other tasks). A test refuses a
/// missing, duplicated or file-less row, and a new routed module fails the grep test below.
#[rustfmt::skip]
pub const ENTRY_POINTS: [EntryRow; 6] = [
    EntryRow { point: EntryPoint::Api, enforced_in: "crates/framework/spot-http/src/api/service.rs", rule: "every authenticated request: the caller's tier and the route's permission", wired: false },
    EntryRow { point: EntryPoint::Realtime, enforced_in: "crates/framework/spot-cli/src/services/realtime.rs", rule: "a subscription is decided when it opens and at every lease boundary", wired: false },
    EntryRow { point: EntryPoint::Jobs, enforced_in: "crates/framework/spot-cli/src/services/jobs.rs", rule: "a job acting for a user is decided for that user; a system job needs no user", wired: false },
    EntryRow { point: EntryPoint::Scripts, enforced_in: "crates/framework/spot-cli/src/services/scripts.rs", rule: "a script runs with the triggering user's decision, never with more", wired: false },
    EntryRow { point: EntryPoint::Controllers, enforced_in: "crates/framework/spot-cli/src/services/controllers.rs", rule: "a whitelisted method is decided for its caller with the method's permission", wired: false },
    EntryRow { point: EntryPoint::Management, enforced_in: "crates/framework/spot-cli/src/services/mgmt_api.rs", rule: "a management command is refused while the node holds no valid lease", wired: false },
];

/// The single offline decision: whether `user` may act at `tier` (with `permission`, when the entry
/// names one) at `now`. The node's [`SnapshotStore`] is one; so is any double in a test.
pub trait OfflineGate {
    /// Decides. A missing fact is a denial.
    fn decide(&self, user: &str, tier: Tier, permission: Option<&str>, now: NodeClock) -> Decision;
}

impl<P: Persistence> OfflineGate for SnapshotStore<P> {
    fn decide(&self, user: &str, tier: Tier, permission: Option<&str>, now: NodeClock) -> Decision {
        match permission {
            Some(permission) => self.authorise_action(user, tier, permission, now),
            None => self.authorise_offline(user, tier, now),
        }
    }
}

/// What every entry point calls: one decision for one request at `point`. The point is part of the
/// signature so a caller cannot reach the gate without naming where the request came from.
#[must_use]
pub fn decide_at(
    _point: EntryPoint,
    gate: &dyn OfflineGate,
    user: &str,
    tier: Tier,
    permission: Option<&str>,
    now: NodeClock,
) -> Decision {
    gate.decide(user, tier, permission, now)
}

fn readiness(hub: Option<Arc<OfflineIdentity>>, problem: Option<&'static str>) -> Arc<Idle> {
    Idle::new(NAME, Level::Optional, move || -> CheckFuture<'static> {
        let hub = hub.clone();
        Box::pin(async move {
            if let Some(code) = problem {
                return Err(format!("{code}: offline identity cannot be issued"));
            }
            match hub.map_or(0, |h| h.stale_nodes().len()) {
                0 => Ok(()),
                n => Err(format!(
                    "{CODE_STALE_NODES}: {n} node(s) hold an older permission epoch"
                )),
            }
        })
    })
}

/// Registers the module: off with `enabled = false`; degraded while the role snapshot or the
/// directory port is missing; otherwise provides [`OfflineIdentity`] to the modules after it.
///
/// # Errors
/// A setting out of range (the key, never a value).
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    if let Some(key) = settings.invalid_key() {
        return Err(Error(format!("services.{NAME}.{key}: out of range")));
    }
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let (roles, facts) = (ctx.port::<dyn RoleSnapshots>(), ctx.port::<dyn HubFacts>());
    let (Some(roles), Some(facts)) = (roles, facts) else {
        let missing = if ctx.port::<dyn RoleSnapshots>().is_none() {
            CODE_NO_ROLES
        } else {
            CODE_NO_DIRECTORY
        };
        ctx.add(readiness(None, Some(missing)));
        return Ok(());
    };
    let tenant = TenantId::new(ctx.label())
        .map_err(|_| Error(format!("services.{NAME}: the site name is not a tenant id")))?;
    let hub = Arc::new(OfflineIdentity::new(tenant, settings, roles, facts));
    ctx.provide::<OfflineIdentity>(Arc::clone(&hub))?;
    ctx.add(readiness(Some(hub), None));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicBool, Ordering};

    use spot_iam_roles::{
        Assignment, Duty, OutsideGuard, Registry, Role as IamRole, build_snapshot_input,
    };
    use spot_offline_identity::{
        DenyReason, MemoryAuditSink, MemoryJournalStore, OfflineJournal, OfflineStamp,
        QuarantineReason,
    };
    use spot_offline_identity_adapters::{MemoryEnvelopes, OfflineQuarantine};
    use spot_permissions::{Right, Rights, Role, RoleGrant};
    use time::OffsetDateTime;

    use super::*;
    use crate::services::roles::RoleData;

    const T0: u64 = 1_000_000_000_000;
    const BOTH: [(&str, &str, &str); 2] =
        [("clerk", "a-clerk", "Clerk"), ("payer", "a-payer", "Payer")];

    fn node() -> NodeId {
        NodeId::new("branch-1").unwrap()
    }

    /// One offline write: author, the assignment it was made under, the tier it claims and the
    /// epoch of the snapshot the node held.
    fn stamp(device_seq: u64, author: &str, assignment: &str, tier: Tier, epoch: u64) -> OfflineStamp {
        OfflineStamp {
            tenant: TenantId::new("acme").unwrap(),
            node: node(),
            author: author.to_owned(),
            assignment_id: assignment.to_owned(),
            tier,
            epoch,
            epoch_age_ms: 10_000,
            device_seq,
        }
    }

    /// The node's journal after it wrote `stamps` in order: the chain entries it syncs.
    fn entries(stamps: &[OfflineStamp]) -> Vec<ChainEntry> {
        let mut journal = OfflineJournal::open(&node(), MemoryJournalStore::default()).unwrap();
        for (n, stamp) in stamps.iter().enumerate() {
            journal.append(stamp, i64::try_from(n).unwrap() + 1).unwrap();
        }
        journal.store().entries.clone()
    }

    fn published(assignments: &[(&str, &str, &str)]) -> Option<Arc<Published>> {
        let role = |name: &str, rights: Vec<Right>| IamRole {
            direct_grants: vec![RoleGrant {
                doctype: "invoice".to_owned(),
                role: Role(name.to_owned()),
                rights: Rights::new(rights),
                if_owner: false,
            }],
            ..IamRole::custom(name)
        };
        let roles = vec![
            role("Clerk", vec![Right::Read]),
            role("Payer", vec![Right::Read, Right::Write]),
        ];
        let registry = Registry::new(Vec::<Duty>::new(), roles).unwrap();
        let list: Vec<Assignment> = assignments
            .iter()
            .map(|(u, id, r)| Assignment::new(id, u, r))
            .collect();
        let input = build_snapshot_input(&registry, &list, OffsetDateTime::UNIX_EPOCH).unwrap();
        let data = RoleData {
            registry,
            assignments: list,
            guard: OutsideGuard::none(),
            ids: Default::default(),
        };
        Some(Arc::new(Published {
            version: 1,
            built_at: OffsetDateTime::UNIX_EPOCH,
            input,
            data,
        }))
    }

    struct World {
        roles: Mutex<Option<Arc<Published>>>,
        down: AtomicBool,
        state: Mutex<Option<NodeState>>,
    }

    impl RoleSnapshots for World {
        fn current(&self) -> Option<Arc<Published>> {
            self.roles.lock().unwrap().clone()
        }
    }

    impl HubFacts for World {
        fn served_users(&self, _node: &NodeId) -> std::result::Result<Vec<ServedUser>, String> {
            if self.down.load(Ordering::SeqCst) {
                return Err("down".to_owned());
            }
            let user = |n: &str, t| ServedUser {
                user: n.to_owned(),
                enabled: true,
                offline_allowed: true,
                highest_tier: t,
            };
            Ok(vec![user("clerk", Tier::Three), user("payer", Tier::Two)])
        }
        fn node_state(&self, _node: &NodeId) -> Option<NodeState> {
            *self.state.lock().unwrap()
        }
        fn key_epoch(&self) -> u64 {
            1
        }
        fn sync_classes(&self) -> Vec<SyncClassEntry> {
            Vec::new()
        }
    }

    fn hub(assignments: &[(&str, &str, &str)]) -> (OfflineIdentity, Arc<World>) {
        let world = Arc::new(World {
            roles: Mutex::new(published(assignments)),
            down: AtomicBool::new(false),
            state: Mutex::new(Some(NodeState::Active)),
        });
        let hub = OfflineIdentity::new(
            TenantId::new("acme").unwrap(),
            Settings::default(),
            Arc::clone(&world) as Arc<dyn RoleSnapshots>,
            Arc::clone(&world) as Arc<dyn HubFacts>,
        );
        (hub, world)
    }

    fn report(epoch: u64) -> NodeReport {
        NodeReport {
            highest_epoch: epoch,
            snapshot_hash: None,
            last_attested: T0,
            hub_nonce: 0,
        }
    }

    #[test]
    fn the_snapshot_is_the_live_role_snapshot_with_its_lease_and_the_same_content_keeps_its_epoch()
    {
        let (hub, world) = hub(&BOTH);
        let first = hub.issue(&node(), T0).unwrap();
        assert_eq!(first.epoch, 1);
        let payer = first.grants.iter().find(|g| g.user == "payer").unwrap();
        assert_eq!(payer.permissions, ["invoice.read", "invoice.write"]);
        assert_eq!(first.valid_until.get(Tier::One), T0 + 3_600_000);
        assert_eq!(first.valid_until.get(Tier::Three), T0 + 24 * 3_600_000);
        assert_eq!(hub.issue(&node(), T0 + 60_000).unwrap().epoch, 1);
        // A reduction raises the epoch; so does an old snapshot.
        *world.roles.lock().unwrap() =
            published(&[("clerk", "a-clerk", "Clerk"), ("payer", "a-payer", "Clerk")]);
        let reduced = hub.issue(&node(), T0 + 120_000).unwrap();
        assert_eq!(reduced.epoch, 2);
        assert_eq!(
            hub.issue(&node(), T0 + 120_000 + 50 * 60_000)
                .unwrap()
                .epoch,
            3
        );
    }

    #[test]
    fn a_node_behind_the_latest_epoch_is_reported_until_it_catches_up() {
        let (hub, world) = hub(&BOTH);
        hub.issue(&node(), T0).unwrap();
        hub.record_report(&node(), &report(1)).unwrap();
        assert!(hub.stale_nodes().is_empty());
        *world.roles.lock().unwrap() = published(&[("clerk", "a-clerk", "Clerk")]);
        hub.issue(&node(), T0 + 1_000).unwrap();
        assert_eq!(
            hub.stale_nodes(),
            [StaleNode {
                node: node(),
                held: 1,
                latest: 2
            }]
        );
        assert!(hub.needs_you()[0].starts_with("offline_identity.stale_snapshot: node branch-1"));
        hub.record_report(&node(), &report(2)).unwrap();
        assert!(hub.stale_nodes().is_empty());
        assert_eq!(
            hub.record_report(&node(), &report(9)),
            Err(IssueError::EpochNeverIssued)
        );
    }

    #[test]
    fn a_write_by_a_user_revoked_after_issue_is_quarantined_once_and_a_forgery_with_it() {
        let (hub, world) = hub(&BOTH);
        hub.issue(&node(), T0).unwrap();
        *world.roles.lock().unwrap() = published(&[("clerk", "a-clerk", "Clerk")]);
        let latest = hub.issue(&node(), T0 + 1_000).unwrap();
        assert!(latest.revocations.iter().any(|r| matches!(r,
            Revocation::User { user, assignment_id: Some(a), .. } if user == "payer" && a == "a-payer")));
        let written = entries(&[
            stamp(1, "clerk", "a-clerk", Tier::Three, 2),
            stamp(2, "payer", "a-payer", Tier::Two, 1),
            stamp(3, "clerk", "a-forged", Tier::Three, 2),
            stamp(4, "clerk", "a-clerk", Tier::Three, 7),
        ]);
        let mut queue = OfflineQuarantine::open(MemoryEnvelopes::default()).unwrap();
        let mut audit = MemoryAuditSink::default();
        let sorted = hub
            .sync(&node(), &written, &mut queue, &mut audit, 2_000)
            .unwrap();
        assert_eq!(sorted.accepted, [1]);
        assert_eq!(
            sorted.quarantined,
            [
                (2, QuarantineReason::AuthorRevoked),
                (3, QuarantineReason::AssignmentMismatch),
                (4, QuarantineReason::UnknownEpoch)
            ]
        );
        assert_eq!(queue.len(), 3);
        let again = hub
            .sync(&node(), &written, &mut queue, &mut audit, 3_000)
            .unwrap();
        assert_eq!(queue.len(), 3);
        assert!(again.accepted.is_empty() && again.quarantined.is_empty());
    }

    struct Lapsed;

    impl OfflineGate for Lapsed {
        fn decide(
            &self,
            _user: &str,
            tier: Tier,
            _permission: Option<&str>,
            _now: NodeClock,
        ) -> Decision {
            Decision::Deny(DenyReason::TierLapsed(tier))
        }
    }

    #[test]
    fn every_entry_point_honours_an_expired_lease_and_the_table_lists_each_once() {
        let now = NodeClock {
            mono_ms: 1,
            boot_id: 1,
        };
        for point in EntryPoint::ALL {
            for tier in Tier::ALL {
                assert!(
                    !decide_at(point, &Lapsed, "clerk", tier, Some("invoice.read"), now)
                        .is_allowed()
                );
            }
        }
        let mut listed: Vec<EntryPoint> = ENTRY_POINTS.iter().map(|r| r.point).collect();
        listed.sort();
        assert_eq!(listed, EntryPoint::ALL);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        for row in ENTRY_POINTS {
            assert!(
                root.join(row.enforced_in).is_file(),
                "{} is not a file",
                row.enforced_in
            );
        }
    }

    /// The grep test: a module of `src/services` that adds routes is either reviewed (its routes are API
    /// routes, decided by the `Api` row) or in the table. A new routed module fails until someone decides.
    #[test]
    fn a_new_routed_module_must_be_reviewed_for_offline_coverage() {
        const REVIEWED: &[&str] = &[
            "durability",
            "events",
            "export",
            "graph",
            "identity_admin",
            "import",
            "invitations",
            "journal_access",
            "link_exchange",
            // A gate layer over every request, not a route: it refuses, it grants nothing.
            "maintenance_gate",
            "mcp",
            "node_role",
            // Authenticated API routes: decided by the `Api` row.
            "occupancy",
            "offline_identity",
            "process_api",
            "process_tasks",
            "process_triggers",
            "process_versions",
            "report",
            "scim",
            "session",
            "sso",
            "sso_identity",
            "sso_logout",
            // Authenticated API route (bearer required): decided by the `Api` row.
            "step_up",
            "telemetry",
            "totp",
            "webhook_secret",
            "workload_routing",
        ];
        let table: Vec<&str> = ENTRY_POINTS
            .iter()
            .filter_map(|r| r.enforced_in.rsplit('/').next())
            .map(|f| f.trim_end_matches(".rs"))
            .collect();
        let mut unreviewed = Vec::new();
        for entry in
            std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services"))
                .unwrap()
        {
            let path = entry.unwrap().path();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_owned();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") || stem == "mod" {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let routed = text.contains("ctx.router_layer(") || text.contains("ctx.open_paths(");
            if routed && !table.contains(&stem.as_str()) && !REVIEWED.contains(&stem.as_str()) {
                unreviewed.push(stem);
            }
        }
        assert!(
            unreviewed.is_empty(),
            "routed modules not in the coverage table or the reviewed list: {unreviewed:?}"
        );
    }
}
