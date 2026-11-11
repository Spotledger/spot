//! The hub-side grant service.

use std::collections::{BTreeMap, BTreeSet};

use spot_audit_chain::{AuditRecord, PayloadValue};
use spot_offline_identity::AuditSink;
use spot_offline_identity::audit::{ACTOR_NODE, actor};
use spot_partition::{PartitionError, RangeAllocator, RangeLease};
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

use crate::allocator::RebuildableAllocator;
use crate::dedupe::DedupeIndex;
use crate::error::GrantError;
use crate::identity::{LeaseRequest, TransportNode};
use crate::log::{GrantLog, GrantRecord};
use crate::policy::GrantPolicy;
use crate::refusals::{RefusalReason, RefusalWindows};
use crate::standing::{Standing, StandingPort};

/// The audit kind of a grant.
pub const KIND_GRANTED: &str = "lease_grants.granted";
/// The audit kind of a refusal.
pub const KIND_REFUSED: &str = "lease_grants.refused";
/// The audit kind of a re-request answered with the range already granted.
pub const KIND_REPLAYED: &str = "lease_grants.replayed";

/// Grants number ranges to branch nodes (WP-EDG-03.T4/T6, R-065).
///
/// # The rules
///
/// 1. **Identity.** The node is the one the transport authenticated ([`TransportNode`]). A
///    request whose payload names another node is refused (`lease_grants.node_mismatch`) and
///    audited with both ids.
/// 2. **Standing.** Only an `Active` node is granted a range. A suspended, decommissioned or
///    unknown node, and a node whose standing cannot be read, gets `lease_grants.node_not_active`
///    (fail closed); the message never says which. The standing is read per request through the
///    [`StandingPort`], so a suspension on the hub is seen by the next request
///    ([`LiveRegistryStanding`](crate::LiveRegistryStanding)).
/// 3. **Entitlement.** A node leases only the series its [`GrantPolicy`] names for it
///    (`lease_grants.series_not_entitled`), and only while the service tracks fewer than the
///    policy's distinct-series cap (`lease_grants.series_cap_reached`). Both checks come before
///    anything is allocated or remembered, so a series a node invented costs the hub nothing.
/// 4. **Size and rate.** A request of zero or above the series maximum of the [`GrantPolicy`] is
///    refused (`size_refused`). A node is granted at most one range per series per
///    `min_interval`, measured on the `at` instants passed in (`too_soon`). The instant of an
///    attempt is remembered as soon as a range is taken, so a failing log or audit sink does
///    not open a faster path. The comparison adds the interval to the last grant's instant with
///    checked arithmetic: a corrupt log record or an absurd instant near the end of the date
///    range is refused as `time_invalid`, never a panic.
/// 5. **Idempotent re-requests.** The same `(node, series, request_id)` is answered with the
///    range already granted to it, read from the log (and from the bounded per-node memory
///    rebuilt out of the log at start), instead of burning a second range. A replay is answered
///    before the size and rate checks because it takes nothing new; it is still audited, as
///    `lease_grants.replayed`, before it is returned. The identity, standing and entitlement
///    checks come first, so a suspended or no-longer-entitled node gets nothing back.
/// 6. **Atomic, persisted before use.** The range is taken from the allocator, written to the
///    [`GrantLog`], audited, and only then returned. If the log or the audit sink fails, nothing
///    is returned and the range is **lost, never reissued**: the cursor never moves backwards,
///    so the next grant is strictly above it. After a restart [`GrantService::open`] rebuilds
///    the cursors, lease ids, last-grant times and idempotency keys from the log.
/// 7. **Audit.** Every outcome writes one record (ids and numbers only) before the answer is
///    returned. A grant whose audit fails is not returned. A refusal is returned whether or not
///    its audit record could be written (it grants nothing either way), and a failed refusal
///    audit is counted in [`GrantService::refusal_audit_failures`] so the hub can see it. A
///    node that refuses in a loop costs at most two records per `(node, series, reason)` per
///    refusal window, the second carrying the window's count.
/// 8. **Honest numbers.** A number that does not fit the audit payload's signed integer is
///    written as its exact decimal text, never saturated to a number it is not.
///
/// The service is synchronous and reads no clock. Wrap it in a `Mutex` to share it: a request
/// takes `&mut self`, so two requests never interleave.
pub struct GrantService<A, L, S, K> {
    allocator: A,
    log: L,
    standing: S,
    audit: K,
    policy: GrantPolicy,
    /// The instant of the last range taken, per `(series, node)`.
    last: BTreeMap<(String, String), OffsetDateTime>,
    /// Every series a grant has named, for the policy's cap on distinct series.
    series: BTreeSet<String>,
    /// The ranges recent requests were answered with, per node.
    dedupe: DedupeIndex,
    /// The open refusal windows, so a refusing node cannot flood the audit log.
    refusals: RefusalWindows,
    /// How many refusal records could not be written.
    refusal_audit_failures: u64,
}

impl<L: GrantLog, S: StandingPort, K: AuditSink> GrantService<RebuildableAllocator, L, S, K> {
    /// A service whose allocator resumes above everything in `log` (the way to start after a
    /// restart as well as the first time).
    pub fn open(log: L, standing: S, audit: K, policy: GrantPolicy) -> Self {
        let allocator = RebuildableAllocator::from_records(&log.all());
        Self::new(allocator, log, standing, audit, policy)
    }
}

impl<A: RangeAllocator, L: GrantLog, S: StandingPort, K: AuditSink> GrantService<A, L, S, K> {
    /// A service over `allocator`.
    ///
    /// The allocator must already start above every range in `log`
    /// ([`RebuildableAllocator::from_records`]); the log refuses an overlapping append, so a
    /// mistake here is caught as `lease_grants.log_failed`, not as a double grant. The
    /// last-grant times, the tracked series and the idempotency keys are rebuilt from the log.
    pub fn new(allocator: A, log: L, standing: S, audit: K, policy: GrantPolicy) -> Self {
        let records = log.all();
        let mut last: BTreeMap<(String, String), OffsetDateTime> = BTreeMap::new();
        let mut series: BTreeSet<String> = BTreeSet::new();
        for r in &records {
            let key = (
                r.lease.series.as_str().to_owned(),
                r.node.as_str().to_owned(),
            );
            let slot = last.entry(key).or_insert(r.granted_at);
            *slot = (*slot).max(r.granted_at);
            series.insert(r.lease.series.as_str().to_owned());
        }
        let dedupe = DedupeIndex::from_records(&records, policy.max_remembered_requests());
        let refusals = RefusalWindows::new(policy.refusal_window());
        Self {
            allocator,
            log,
            standing,
            audit,
            policy,
            last,
            series,
            dedupe,
            refusals,
            refusal_audit_failures: 0,
        }
    }

    /// The grant log.
    pub fn log(&self) -> &L {
        &self.log
    }

    /// The audit sink.
    pub fn audit(&self) -> &K {
        &self.audit
    }

    /// The grant log, mutably (wiring and tests; the service is the only writer of records).
    pub fn log_mut(&mut self) -> &mut L {
        &mut self.log
    }

    /// The audit sink, mutably (wiring and tests).
    pub fn audit_mut(&mut self) -> &mut K {
        &mut self.audit
    }

    /// The policy in force.
    pub fn policy(&self) -> &GrantPolicy {
        &self.policy
    }

    /// How many refusal records could not be written.
    pub fn refusal_audit_failures(&self) -> u64 {
        self.refusal_audit_failures
    }

    /// How many distinct series the service tracks (against the policy's cap).
    pub fn tracked_series(&self) -> usize {
        self.series.len()
    }

    /// How many requests are remembered across all nodes, for idempotent re-requests.
    pub fn remembered_requests(&self) -> usize {
        self.dedupe.len()
    }

    /// How many refusal windows are open.
    pub fn open_refusal_windows(&self) -> usize {
        self.refusals.len()
    }

    /// Takes the service apart, for a restart that keeps the log (and the standing and audit
    /// stores) and rebuilds the rest with [`GrantService::open`].
    pub fn into_parts(self) -> (A, L, S, K) {
        (self.allocator, self.log, self.standing, self.audit)
    }

    /// The standing port (so the hub can suspend a node).
    pub fn standing_mut(&mut self) -> &mut S {
        &mut self.standing
    }

    /// Grants the authenticated node a range, or refuses. See the type's rules.
    ///
    /// # Errors
    ///
    /// [`GrantError`], with a stable `lease_grants.*` code.
    pub fn request(
        &mut self,
        who: &TransportNode,
        request: &LeaseRequest,
        at: OffsetDateTime,
    ) -> Result<RangeLease, GrantError> {
        self.decide(who, request, at)
    }

    fn decide(
        &mut self,
        who: &TransportNode,
        request: &LeaseRequest,
        at: OffsetDateTime,
    ) -> Result<RangeLease, GrantError> {
        let node = who.node();
        if let Some(claimed) = &request.claimed_node
            && claimed != node
        {
            return Err(self.refuse(
                who,
                request,
                at,
                RefusalReason::NodeMismatch,
                GrantError::NodeMismatch {
                    authenticated: node.clone(),
                    claimed: claimed.clone(),
                },
            ));
        }
        match self.standing.standing(node) {
            Ok(Standing::Active) => {}
            Ok(_) => {
                return Err(self.refuse(
                    who,
                    request,
                    at,
                    RefusalReason::NodeNotActive,
                    GrantError::NodeNotActive,
                ));
            }
            Err(_) => {
                return Err(self.refuse(
                    who,
                    request,
                    at,
                    RefusalReason::StandingUnavailable,
                    GrantError::NodeNotActive,
                ));
            }
        }
        // The hub decides which series a node may lease, and how many series it tracks at all.
        // Both refusals come before anything is allocated or remembered.
        if !self.policy.entitles(node, &request.series) {
            return Err(self.refuse(
                who,
                request,
                at,
                RefusalReason::SeriesNotEntitled,
                GrantError::SeriesNotEntitled,
            ));
        }
        if !self.series.contains(request.series.as_str())
            && self.series.len() >= self.policy.max_series()
        {
            return Err(self.refuse(
                who,
                request,
                at,
                RefusalReason::SeriesCapReached,
                GrantError::SeriesCapReached {
                    max: self.policy.max_series(),
                },
            ));
        }
        // A re-request of a range already granted under this envelope id: answer with that
        // range. It is done before the size and rate checks because it takes nothing new.
        if let Some(lease) = self.dedupe.lookup(
            node.as_str(),
            request.series.as_str(),
            request.request_id.as_ref(),
        ) {
            return self.replay(who, request, at, lease);
        }
        let max = self.policy.max_size(&request.series);
        if request.size == 0 || request.size > max {
            return Err(self.refuse(
                who,
                request,
                at,
                RefusalReason::SizeRefused,
                GrantError::SizeRefused { max },
            ));
        }
        let key = (request.series.as_str().to_owned(), node.as_str().to_owned());
        if let Some(last) = self.last.get(&key).copied() {
            // Checked: a corrupt `granted_at` or an absurd `at` near the end of the date range
            // must be refused, not panic the hub.
            let Some(due) = last.checked_add(self.policy.min_interval()) else {
                return Err(self.refuse(
                    who,
                    request,
                    at,
                    RefusalReason::TimeInvalid,
                    GrantError::TimeInvalid,
                ));
            };
            if at < due {
                return Err(self.refuse(
                    who,
                    request,
                    at,
                    RefusalReason::TooSoon,
                    GrantError::TooSoon,
                ));
            }
        }

        let lease = match self
            .allocator
            .lease(&request.series, node, request.size, at)
        {
            Ok(lease) => lease,
            Err(PartitionError::RangeExhausted { .. }) => {
                return Err(self.refuse(
                    who,
                    request,
                    at,
                    RefusalReason::SeriesExhausted,
                    GrantError::SeriesExhausted,
                ));
            }
            Err(_) => {
                return Err(self.refuse(
                    who,
                    request,
                    at,
                    RefusalReason::AllocatorFailed,
                    GrantError::AllocatorFailed,
                ));
            }
        };
        // From here the range is spent whatever happens: it is never returned to the allocator.
        self.last.insert(key, at);
        self.series.insert(request.series.as_str().to_owned());
        let record = GrantRecord {
            lease: lease.clone(),
            node: node.clone(),
            requested_size: request.size,
            granted_at: at,
            request_id: request.request_id.clone(),
        };
        if self.log.append(record).is_err() {
            return Err(self.refuse(
                who,
                request,
                at,
                RefusalReason::LogFailed,
                GrantError::LogFailed,
            ));
        }
        // Only now is the range one a retry may be answered with: it is durable.
        if let Some(id) = &request.request_id {
            self.dedupe
                .remember(node.as_str(), request.series.as_str(), id, lease.clone());
        }
        let granted = audit_record(
            KIND_GRANTED,
            node,
            at,
            [
                (
                    "series",
                    PayloadValue::Text(lease.series.as_str().to_owned()),
                ),
                ("from", number(lease.from)),
                ("to", number(lease.to)),
                ("lease_id", number(lease.lease_id.get())),
                ("requested_size", number(request.size)),
                ("request_id", PayloadValue::Text(request_id_text(request))),
            ],
        );
        if self.audit.record(granted).is_err() {
            // The range is in the log and unused: lost, never reissued.
            return Err(GrantError::AuditFailed);
        }
        Ok(lease)
    }

    /// Answers a re-request with the range already granted, after auditing the replay.
    fn replay(
        &mut self,
        who: &TransportNode,
        request: &LeaseRequest,
        at: OffsetDateTime,
        lease: RangeLease,
    ) -> Result<RangeLease, GrantError> {
        let record = audit_record(
            KIND_REPLAYED,
            who.node(),
            at,
            [
                (
                    "series",
                    PayloadValue::Text(lease.series.as_str().to_owned()),
                ),
                ("from", number(lease.from)),
                ("to", number(lease.to)),
                ("lease_id", number(lease.lease_id.get())),
                ("requested_size", number(request.size)),
                ("request_id", PayloadValue::Text(request_id_text(request))),
            ],
        );
        if self.audit.record(record).is_err() {
            // Same rule as a grant: nothing leaves an unaudited service.
            return Err(GrantError::AuditFailed);
        }
        Ok(lease)
    }

    /// Audits a refusal (counting it in its window) and returns the error to hand back.
    fn refuse(
        &mut self,
        who: &TransportNode,
        request: &LeaseRequest,
        at: OffsetDateTime,
        reason: RefusalReason,
        error: GrantError,
    ) -> GrantError {
        let booked = self
            .refusals
            .book(who.node(), request.series.as_str(), reason, at);
        if let Some(count) = booked.closed {
            let record = refusal_record(who, request, at, reason, &error, count, true);
            self.write_refusal(record);
        }
        if let Some(count) = booked.opened {
            let record = refusal_record(who, request, at, reason, &error, count, false);
            self.write_refusal(record);
        }
        error
    }

    /// Writes one refusal record, counting a sink failure the hub can read.
    fn write_refusal(&mut self, record: AuditRecord) {
        // A refusal grants nothing, so a sink failure changes nothing about the answer; it is
        // counted so a hub whose audit store is down can see that refusals went unrecorded.
        if self.audit.record(record).is_err() {
            self.refusal_audit_failures = self.refusal_audit_failures.saturating_add(1);
        }
    }
}

/// The audit record of one refusal: the window's first record, or the aggregate closing one.
fn refusal_record(
    who: &TransportNode,
    request: &LeaseRequest,
    at: OffsetDateTime,
    reason: RefusalReason,
    error: &GrantError,
    count: u64,
    aggregate: bool,
) -> AuditRecord {
    let claimed = request
        .claimed_node
        .as_ref()
        .map_or_else(String::new, |n| n.as_str().to_owned());
    audit_record(
        KIND_REFUSED,
        who.node(),
        at,
        [
            ("reason", PayloadValue::Text(reason.as_str().to_owned())),
            ("code", PayloadValue::Text(error.code().to_owned())),
            (
                "series",
                PayloadValue::Text(request.series.as_str().to_owned()),
            ),
            ("requested_size", number(request.size)),
            ("claimed_node", PayloadValue::Text(claimed)),
            ("request_id", PayloadValue::Text(request_id_text(request))),
            ("count", number(count)),
            ("aggregate", PayloadValue::Bool(aggregate)),
        ],
    )
}

/// The request id as audit text: the id, or empty when the envelope carried none.
fn request_id_text(request: &LeaseRequest) -> String {
    request
        .request_id
        .as_ref()
        .map_or_else(String::new, |id| id.as_str().to_owned())
}

/// A number for the audit payload: the value while it fits, else its exact decimal text.
///
/// The audit chain's integer payload is signed, and a `u64` above `i64::MAX` (a series cursor
/// near the end of the number space) must never be recorded as a different number.
fn number(v: u64) -> PayloadValue {
    match i64::try_from(v) {
        Ok(n) => PayloadValue::Int(n),
        Err(_) => PayloadValue::Text(v.to_string()),
    }
}

fn micros(at: OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
}

/// The record of one outcome: the authenticated node as actor, ids and numbers as payload.
fn audit_record<const N: usize>(
    kind: &str,
    node: &NodeId,
    at: OffsetDateTime,
    fields: [(&str, PayloadValue); N],
) -> AuditRecord {
    AuditRecord {
        kind: kind.to_owned(),
        actor: actor(ACTOR_NODE, node.as_str()),
        on_behalf_of: None,
        subject: None,
        at: micros(at),
        payload: fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        masked: true,
    }
}
