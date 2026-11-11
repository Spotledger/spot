//! Fail-closed authorisation on the node (ADR-070 rule 12.13 c and d).
//!
//! Every check must hold for [`SnapshotStore::authorise_offline`] to allow; a missing fact is a
//! denial. In order:
//!
//! 1. a verified snapshot is held and does not suspend or decommission this node;
//! 2. the node certificate is good;
//! 3. the monotonic clock is sane;
//! 4. the user is in the directory, enabled, on the offline allowlist and not revoked;
//! 5. the requested tier is one the user may act at offline;
//! 6. the lease of the user's own highest tier has not lapsed (the node can still *confirm*
//!    the user: a payments user stops on the short bound while a clerk carries on) and the
//!    lease of the requested tier has not lapsed.
//!
//! A supervisor holding `sync.override_stale` may substitute for step 6 for a bounded time
//! ([`SnapshotStore::request_override`]); the override never touches steps 1 to 5, it is
//! written to the audit sink **before** it takes effect, it is not persisted (a reboot ends
//! it), and tier 1 cannot be overridden under the default policy. Its grant is bounded twice:
//! by the policy's maximum and by the tier's own bound (the tier's signed lease length), so a
//! grant longer than the tier it rescues is refused.

use spot_audit_chain::PayloadValue;

use crate::audit::{ACTOR_USER, AuditSink, actor, audit_record, int, text};
use crate::error::{DenyReason, OfflineError};
use crate::lease::Lease;
use crate::snapshot::{DirectoryEntry, NodeRevocationKind, Revocation, Snapshot};
use crate::store::{Persistence, SnapshotStore};
use crate::types::{NodeClock, Tier};

/// The permission that lets a supervisor override a stale lease.
pub const OVERRIDE_PERMISSION: &str = concat!("sync", ".override_stale");

const MAX_REASON_CHARS: usize = 256;

/// The answer to an authorisation question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The action may proceed.
    Allow,
    /// The action may not proceed.
    Deny(DenyReason),
}

impl Decision {
    /// Whether this is [`Decision::Allow`].
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// How far an override may go. Both limits apply: the policy's maximum and the tier's own
/// bound (the tier's signed lease length, rule 12.13 d).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverridePolicy {
    /// The longest one override may last, in milliseconds.
    pub max_duration_ms: u64,
    /// Whether tier 1 (payments, approvals, administrator actions, `reveal`) may be overridden.
    pub tier1_overridable: bool,
}

impl Default for OverridePolicy {
    fn default() -> Self {
        Self {
            max_duration_ms: 3_600_000,
            tier1_overridable: false,
        }
    }
}

/// The overrides in force. In memory only, so a reboot ends them.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Overrides {
    policy: OverridePolicy,
    active: [Option<Lease>; 3],
}

impl Overrides {
    fn is_active(&self, tier: Tier, now: NodeClock) -> bool {
        self.active[tier.index()].is_some_and(|l| l.remaining(Tier::One, now) > 0)
    }
}

/// A supervisor's request for an override.
#[derive(Clone, Copy, Debug)]
pub struct OverrideRequest<'a> {
    /// The supervisor, as signed in on the node.
    pub supervisor: &'a str,
    /// The tier whose stale lease is overridden.
    pub tier: Tier,
    /// How long, in milliseconds.
    pub duration_ms: u64,
    /// Why, in the supervisor's words (at most 256 characters). Never put a secret in it.
    pub reason: &'a str,
    /// Audit timestamp in microseconds; never used for any lease.
    pub at_micros: i64,
}

/// What an override gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverrideGrant {
    /// The tier.
    pub tier: Tier,
    /// How long it lasts.
    pub duration_ms: u64,
}

fn node_suspended(snapshot: &Snapshot) -> bool {
    snapshot.revocations.iter().any(|r| {
        matches!(r, Revocation::Node { node, kind, .. }
            if node == snapshot.node.as_str()
                && matches!(kind, NodeRevocationKind::Suspended | NodeRevocationKind::Decommissioned))
    })
}

fn user_revoked(snapshot: &Snapshot, user: &str) -> bool {
    snapshot
        .revocations
        .iter()
        .any(|r| matches!(r, Revocation::User { user: u, assignment_id: None, .. } if u == user))
}

fn assignment_revoked(snapshot: &Snapshot, user: &str, assignment: &str) -> bool {
    snapshot.revocations.iter().any(|r| {
        matches!(r, Revocation::User { user: u, assignment_id: Some(a), .. }
            if u == user && a == assignment)
    })
}

impl<P: Persistence> SnapshotStore<P> {
    /// Sets the override policy. The default is one hour at most and no tier 1.
    pub fn set_override_policy(&mut self, policy: OverridePolicy) {
        self.overrides.policy = policy;
    }

    /// The snapshot, if the node's state allows trusting it at all (steps 1 to 3).
    fn trusted_snapshot(&self, now: NodeClock) -> Result<&Snapshot, DenyReason> {
        let snapshot = self.snapshot().ok_or(DenyReason::NoSnapshot)?;
        if node_suspended(snapshot) {
            return Err(DenyReason::NodeSuspended);
        }
        if !self.lease.clock_ok(now) {
            return Err(DenyReason::ClockFault);
        }
        if !self.cert.is_valid(now) {
            return Err(DenyReason::CertificateLapsed);
        }
        Ok(snapshot)
    }

    /// The user's directory entry, if the snapshot lets them work offline (step 4).
    fn confirmed_user<'a>(
        snapshot: &'a Snapshot,
        user: &str,
    ) -> Result<&'a DirectoryEntry, DenyReason> {
        let entry = snapshot
            .directory
            .iter()
            .find(|d| d.user == user)
            .ok_or(DenyReason::UnknownUser)?;
        if !entry.enabled {
            return Err(DenyReason::UserDisabled);
        }
        if !entry.offline_allowed {
            return Err(DenyReason::NotOfflineAllowed);
        }
        if user_revoked(snapshot, user) {
            return Err(DenyReason::UserRevoked);
        }
        Ok(entry)
    }

    fn tier_alive(&self, tier: Tier, now: NodeClock) -> bool {
        self.lease.remaining(tier, now) > 0 || self.overrides.is_active(tier, now)
    }

    fn check_user(&self, user: &str, tier: Option<Tier>, now: NodeClock) -> Result<(), DenyReason> {
        let snapshot = self.trusted_snapshot(now)?;
        let entry = Self::confirmed_user(snapshot, user)?;
        if let Some(requested) = tier {
            if requested.index() < entry.highest_tier.index() {
                return Err(DenyReason::TierNotAllowedForUser);
            }
            if !self.tier_alive(requested, now) {
                return Err(DenyReason::TierLapsed(requested));
            }
        }
        if !self.tier_alive(entry.highest_tier, now) {
            return Err(DenyReason::TierLapsed(entry.highest_tier));
        }
        Ok(())
    }

    /// Whether `user` may sign in on this node at `now`: the node can still confirm them as
    /// enabled (freshness of their own highest tier).
    #[must_use]
    pub fn sign_in(&self, user: &str, now: NodeClock) -> Decision {
        match self.check_user(user, None, now) {
            Ok(()) => Decision::Allow,
            Err(reason) => Decision::Deny(reason),
        }
    }

    /// Whether `user` may act at `tier` while offline at `now`.
    #[must_use]
    pub fn authorise_offline(&self, user: &str, tier: Tier, now: NodeClock) -> Decision {
        match self.check_user(user, Some(tier), now) {
            Ok(()) => Decision::Allow,
            Err(reason) => Decision::Deny(reason),
        }
    }

    /// [`Self::authorise_offline`] and, as well, `permission` is granted to `user` by at least
    /// one assignment the snapshot has not revoked.
    #[must_use]
    pub fn authorise_action(
        &self,
        user: &str,
        tier: Tier,
        permission: &str,
        now: NodeClock,
    ) -> Decision {
        if let Err(reason) = self.check_user(user, Some(tier), now) {
            return Decision::Deny(reason);
        }
        if self.holds(user, permission) {
            Decision::Allow
        } else {
            Decision::Deny(DenyReason::PermissionNotGranted)
        }
    }

    fn holds(&self, user: &str, permission: &str) -> bool {
        self.snapshot().is_some_and(|s| {
            s.grants.iter().any(|g| {
                g.user == user
                    && !assignment_revoked(s, user, &g.assignment_id)
                    && g.permissions.iter().any(|p| p == permission)
            })
        })
    }

    /// Grants a bounded override of a stale tier lease. The audit record is written first; if
    /// it cannot be written, nothing is granted. A refusal is audited too (best effort).
    ///
    /// # Errors
    /// [`OfflineError::OverrideRefused`] when the supervisor, the tier, the duration or the
    /// reason is not acceptable; [`OfflineError::AuditFailed`] when the audit sink failed.
    pub fn request_override<A: AuditSink>(
        &mut self,
        request: &OverrideRequest<'_>,
        now: NodeClock,
        audit: &mut A,
    ) -> Result<OverrideGrant, OfflineError> {
        if let Err(reason) = self.check_override(request, now) {
            let record =
                override_record("offline_identity.override_refused", request, Some(reason));
            let _ = audit.record(record);
            return Err(OfflineError::OverrideRefused { reason });
        }
        let record = override_record(
            concat!("offline_identity", ".override_granted"),
            request,
            None,
        );
        audit
            .record(record)
            .map_err(|_| OfflineError::AuditFailed)?;
        self.overrides.active[request.tier.index()] =
            Some(Lease::start([request.duration_ms, 0, 0], now));
        Ok(OverrideGrant {
            tier: request.tier,
            duration_ms: request.duration_ms,
        })
    }

    /// The tier's own bound: the signed `valid_until` of the tier minus the snapshot's
    /// `issued_at` (rule 12.13 d: a group's key lease equals its tier bound). It is the bound
    /// the hub measures a write's `epoch_age` against on sync, so an override must stay
    /// inside it too.
    fn tier_bound(&self, tier: Tier) -> Option<u64> {
        let snapshot = self.snapshot()?;
        Some(
            snapshot
                .valid_until
                .get(tier)
                .saturating_sub(snapshot.issued_at),
        )
    }

    fn check_override(
        &self,
        request: &OverrideRequest<'_>,
        now: NodeClock,
    ) -> Result<(), &'static str> {
        let policy = self.overrides.policy;
        if request.tier == Tier::One && !policy.tier1_overridable {
            return Err("tier 1 cannot be overridden");
        }
        if request.duration_ms == 0 || request.duration_ms > policy.max_duration_ms {
            return Err("the duration is zero or beyond the policy maximum");
        }
        // The tier's own bound still holds. An override stands in for step 6 only for as long
        // as the tier it rescues could itself have run: a payments-tier action cannot be
        // rescued by a grant longer than the payments bound, however generous the policy.
        let bound = self
            .tier_bound(request.tier)
            .ok_or("the node holds no snapshot")?;
        if request.duration_ms > bound {
            return Err("the duration is beyond the tier's bound");
        }
        let reason_chars = request.reason.chars().count();
        if reason_chars == 0 || reason_chars > MAX_REASON_CHARS {
            return Err("a reason of 1 to 256 characters is required");
        }
        let snapshot = self
            .trusted_snapshot(now)
            .map_err(|_| "the node state is not trusted")?;
        Self::confirmed_user(snapshot, request.supervisor)
            .map_err(|_| "the supervisor is not an enabled, offline-allowed user")?;
        if !self.holds(request.supervisor, OVERRIDE_PERMISSION) {
            return Err("the supervisor does not hold sync.override_stale");
        }
        Ok(())
    }
}

fn override_record(
    kind: &str,
    request: &OverrideRequest<'_>,
    refusal: Option<&str>,
) -> spot_audit_chain::AuditRecord {
    let mut fields: Vec<(&str, PayloadValue)> = vec![
        ("tier", text(request.tier.as_str())),
        ("duration_ms", int(request.duration_ms)),
        ("reason", text(request.reason)),
    ];
    if let Some(why) = refusal {
        fields.push(("refusal", text(why)));
    }
    audit_record(
        kind,
        &actor(ACTOR_USER, request.supervisor),
        request.at_micros,
        &fields,
    )
}
