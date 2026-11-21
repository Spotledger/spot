//! The hub half of offline-identity replay detection.
//!
//! A branch node asks the hub for signed hub time or a snapshot with a signed
//! [`NodeRequest`](spot_offline_identity::signing::NodeRequest) that says what the node holds
//! (a [`NodeReport`]: highest epoch, snapshot hash, newest hub clock, newest hub nonce). The
//! [`ReplayGuard`] is the hub's answer, in three steps:
//!
//! 1. [`ReplayGuard::admit`] checks the request: the node is the one the transport
//!    authenticated, the signature verifies under that node's key, the node is not flagged, and
//!    the report is not **less** than what the hub knows (a regression) and not **more** than
//!    the hub ever issued (a report from nowhere). A regression flags the node for
//!    re-enrolment, writes `offline_identity.node_regressed` and refuses. The flag is read by
//!    [`ReenrolmentStanding`], so no range lease is granted to a flagged node either.
//! 2. A reply is built from the admitted request: [`ReplayGuard::reply_hub_time`],
//!    [`ReplayGuard::reply_snapshot`] (a snapshot **paired with** a signed hub time, so the node
//!    gets a fresh nonce with it) or [`ReplayGuard::reply_timestamp`]. Each carries a nonce
//!    strictly above every nonce issued to that node before, and the hub time never goes back.
//! 3. Only after the reply is built and signed is the ledger written (compare-and-set), so a
//!    reply that could not be recorded is never sent, and an admitted request answers at most
//!    one reply.
//!
//! [`ReplayGuard::reenrol`] is the operator's way back: it audits, clears the flag, forgets the
//! acknowledged epoch (a node reinstalled from nothing holds none) and keeps every nonce and
//! hub-time high-water mark, so a reply from before the re-enrolment can never be replayed as
//! new. The audit trail is never deleted.
//!
//! The crate reads no clock and holds no key material: signing keys are passed in behind the
//! [`Signer`] trait for one reply, and the audit payload carries ids, numbers and the public
//! SHA-256 of a snapshot, never a key or a signature.

use std::fmt::Write as _;

use spot_audit_chain::{AuditRecord, PayloadValue};
use spot_bundle_signing::PublicKey;
use spot_offline_identity::audit::{ACTOR_NODE, ACTOR_USER, actor};
use spot_offline_identity::signing::{
    HubTime, NodeReport, RequestKind, RequestRefusal, SignedNodeRequest, verify_node_request,
};
use spot_offline_identity::{
    AuditSink, HubMs, NodeId, SignedDelegation, SignedHubTime, SignedSnapshot, SignedTimestamp,
    Signer, SnapshotHash, TenantId, TierDeadlines, Timestamp, sign_hub_time, sign_timestamp,
};
use thiserror::Error;
use time::OffsetDateTime;

use crate::identity::TransportNode;
use crate::ledger::{LedgerStore, NodeLedger};
use crate::standing::{Standing, StandingPort, StandingUnavailable};

/// The audit kind written when a node reports less than the hub gave it.
pub const KIND_NODE_REGRESSED: &str = "offline_identity.node_regressed";
/// The audit kind written when a node reports more than the hub ever issued.
pub const KIND_NODE_REPORT_AHEAD: &str = "offline_identity.node_report_ahead";
/// The audit kind written when an operator re-enrols a flagged node.
pub const KIND_NODE_REENROLLED: &str = "offline_identity.node_reenrolled";

/// Why a request or a reply was refused. A message never carries a key or a signature.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ReplayRefusal {
    /// The request names another node or tenant than the one the transport authenticated.
    #[error("the request does not match the authenticated node ({code})", code = self.code())]
    NodeMismatch,
    /// The request is unsigned, carries no report, or does not verify.
    #[error("{0}")]
    Request(RequestRefusal),
    /// The node reported less than the hub gave it, or is already flagged: it must re-enrol.
    #[error("the node reported an older state than the hub issued and must re-enrol ({code})", code = self.code())]
    NodeRegressed,
    /// The node reported more than the hub ever issued to it.
    #[error("the node reported a state the hub never issued ({code})", code = self.code())]
    ReportAhead,
    /// The reply cannot be built: wrong kind for the request, a snapshot older than or
    /// different from the one acknowledged, a hub time that does not advance, or an exhausted
    /// nonce.
    #[error("the reply cannot be issued for this request ({code})", code = self.code())]
    ReplyRefused,
    /// The ledger could not be read or written; nothing was issued.
    #[error("the node ledger is unavailable, so nothing was issued ({code})", code = self.code())]
    LedgerFailed,
    /// The audit record could not be written; nothing was issued.
    #[error("the decision could not be audited, so nothing was issued ({code})", code = self.code())]
    AuditFailed,
}

impl ReplayRefusal {
    /// The stable dotted code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NodeMismatch => "lease_grants.node_mismatch",
            Self::Request(r) => r.code(),
            Self::NodeRegressed => "lease_grants.node_regressed",
            Self::ReportAhead => "lease_grants.report_ahead",
            Self::ReplyRefused => "lease_grants.reply_refused",
            Self::LedgerFailed => "lease_grants.log_failed",
            Self::AuditFailed => "lease_grants.audit_failed",
        }
    }
}

/// A request that passed [`ReplayGuard::admit`]. It is consumed by exactly one reply.
#[derive(Debug)]
pub struct Admitted {
    node: NodeId,
    kind: RequestKind,
    report: NodeReport,
    ledger: NodeLedger,
}

impl Admitted {
    /// The node.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// What the node asked for.
    #[must_use]
    pub fn kind(&self) -> RequestKind {
        self.kind
    }

    /// What the node said it holds.
    #[must_use]
    pub fn report(&self) -> &NodeReport {
        &self.report
    }
}

/// A snapshot and the signed hub time that goes with it. The node takes both: the snapshot
/// carries no nonce of its own, the time does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotReply {
    /// The snapshot.
    pub snapshot: SignedSnapshot,
    /// The hub time, with a fresh nonce, paired with it.
    pub time: SignedHubTime,
}

/// The hub's replay guard for offline identity.
pub struct ReplayGuard<L, K> {
    tenant: TenantId,
    ledger: L,
    audit: K,
    audit_failures: u64,
    refused_while_flagged: u64,
}

impl<L: LedgerStore, K: AuditSink> ReplayGuard<L, K> {
    /// A guard for `tenant`'s nodes over `ledger`, auditing to `audit`.
    pub fn new(tenant: TenantId, ledger: L, audit: K) -> Self {
        Self {
            tenant,
            ledger,
            audit,
            audit_failures: 0,
            refused_while_flagged: 0,
        }
    }

    /// The ledger store.
    pub fn ledger(&self) -> &L {
        &self.ledger
    }

    /// The audit sink.
    pub fn audit(&self) -> &K {
        &self.audit
    }

    /// The audit sink, mutably (tests).
    pub fn audit_mut(&mut self) -> &mut K {
        &mut self.audit
    }

    /// How many refusal records could not be written. A refusal is returned either way.
    pub fn audit_failures(&self) -> u64 {
        self.audit_failures
    }

    /// How many requests were refused only because the node is flagged (not audited one by
    /// one, so a flagged node cannot flood the audit log; the regression itself was).
    pub fn refused_while_flagged(&self) -> u64 {
        self.refused_while_flagged
    }

    /// Whether `node` is flagged for re-enrolment. An unreadable ledger reads as flagged.
    pub fn requires_reenrolment(&self, node: &NodeId) -> bool {
        self.ledger
            .get(node)
            .map_or(true, |ledger| ledger.requires_reenrolment)
    }

    /// Checks a signed request from the node the transport authenticated.
    ///
    /// # Errors
    ///
    /// [`ReplayRefusal`]: the request does not match the connection, is unsigned or lacks its
    /// report, the node is flagged, the report is a regression (the node is flagged and the
    /// decision audited), the report is ahead of what the hub issued (audited), or the ledger
    /// or audit sink failed.
    pub fn admit(
        &mut self,
        who: &TransportNode,
        node_key: &PublicKey,
        signed: &SignedNodeRequest,
        at: OffsetDateTime,
    ) -> Result<Admitted, ReplayRefusal> {
        let request = &signed.request;
        if request.node.as_str() != who.node().as_str() || request.tenant != self.tenant {
            return Err(ReplayRefusal::NodeMismatch);
        }
        let report = *verify_node_request(node_key, signed).map_err(ReplayRefusal::Request)?;
        let node = request.node.clone();
        let ledger = self
            .ledger
            .get(&node)
            .map_err(|_| ReplayRefusal::LedgerFailed)?;
        if ledger.requires_reenrolment {
            self.refused_while_flagged = self.refused_while_flagged.saturating_add(1);
            return Err(ReplayRefusal::NodeRegressed);
        }
        match judge(&ledger, &report) {
            Verdict::Regressed => {
                // Flag first: the refusal must hold even if the audit sink is down.
                let flagged = NodeLedger {
                    requires_reenrolment: true,
                    ..ledger
                };
                self.ledger
                    .replace(&node, &ledger, flagged)
                    .map_err(|_| ReplayRefusal::LedgerFailed)?;
                self.audit_refusal(KIND_NODE_REGRESSED, &node, &ledger, &report, at);
                Err(ReplayRefusal::NodeRegressed)
            }
            Verdict::Ahead => {
                self.audit_refusal(KIND_NODE_REPORT_AHEAD, &node, &ledger, &report, at);
                Err(ReplayRefusal::ReportAhead)
            }
            Verdict::Accept => {
                let next = NodeLedger {
                    reported_nonce: ledger.reported_nonce.max(report.hub_nonce),
                    reported_attested: ledger.reported_attested.max(report.last_attested),
                    ..ledger
                };
                self.ledger
                    .replace(&node, &ledger, next)
                    .map_err(|_| ReplayRefusal::LedgerFailed)?;
                Ok(Admitted {
                    node,
                    kind: request.kind,
                    report,
                    ledger: next,
                })
            }
        }
    }

    /// Answers a [`RequestKind::HubTime`] request with signed hub time at `hub_ms`.
    ///
    /// The time restates the acknowledged epoch, extends no deadline, and carries a fresh
    /// nonce.
    ///
    /// # Errors
    ///
    /// [`ReplayRefusal::ReplyRefused`] for a request of another kind, a `hub_ms` that does not
    /// advance, or an exhausted nonce; [`ReplayRefusal::LedgerFailed`] when the ledger write
    /// fails (nothing is issued).
    pub fn reply_hub_time(
        &mut self,
        admitted: Admitted,
        timestamp_key: &dyn Signer,
        delegation: SignedDelegation,
        hub_ms: HubMs,
    ) -> Result<SignedHubTime, ReplayRefusal> {
        if admitted.kind != RequestKind::HubTime {
            return Err(ReplayRefusal::ReplyRefused);
        }
        let (nonce, next) = next_issue(&admitted.ledger, hub_ms)?;
        let signed = sign_hub_time(
            HubTime {
                tenant: self.tenant.clone(),
                node: admitted.node.clone(),
                epoch: admitted.ledger.acked_epoch,
                hub_ms,
                nonce,
            },
            timestamp_key,
            delegation,
        );
        self.commit(&admitted, next)?;
        Ok(signed)
    }

    /// Answers a [`RequestKind::Snapshot`] request with `snapshot`, paired with signed hub
    /// time at `hub_ms` that carries the fresh nonce. The acknowledged epoch and hash move to
    /// the snapshot's, after both are built.
    ///
    /// # Errors
    ///
    /// [`ReplayRefusal::ReplyRefused`] for a request of another kind; a snapshot for another
    /// tenant or node; an epoch below the acknowledged one, or the same epoch with another
    /// hash (the hub never forks itself); a `hub_ms` that does not advance or that is before
    /// the snapshot's `issued_at`; [`ReplayRefusal::LedgerFailed`] when the write fails.
    pub fn reply_snapshot(
        &mut self,
        admitted: Admitted,
        snapshot: SignedSnapshot,
        timestamp_key: &dyn Signer,
        hub_ms: HubMs,
    ) -> Result<SnapshotReply, ReplayRefusal> {
        let body = &snapshot.snapshot;
        let hash = body.hash();
        let held = &admitted.ledger;
        if admitted.kind != RequestKind::Snapshot
            || body.tenant != self.tenant
            || body.node != admitted.node
            || body.epoch < held.acked_epoch
            || (body.epoch == held.acked_epoch && held.acked_hash.is_some_and(|h| h != hash))
            || hub_ms < body.issued_at
        {
            return Err(ReplayRefusal::ReplyRefused);
        }
        let (nonce, mut next) = next_issue(held, hub_ms)?;
        next.acked_epoch = body.epoch;
        next.acked_hash = Some(hash);
        let time = sign_hub_time(
            HubTime {
                tenant: self.tenant.clone(),
                node: admitted.node.clone(),
                epoch: body.epoch,
                hub_ms,
                nonce,
            },
            timestamp_key,
            snapshot.delegation.clone(),
        );
        self.commit(&admitted, next)?;
        Ok(SnapshotReply { snapshot, time })
    }

    /// Answers a [`RequestKind::Snapshot`] request with a timestamp that restates the snapshot
    /// the hub last acknowledged, with fresh deadlines and nonce.
    ///
    /// # Errors
    ///
    /// [`ReplayRefusal::ReplyRefused`] for a request of another kind, when no snapshot was
    /// ever acknowledged, for a `hub_ms` that does not advance, or an exhausted nonce;
    /// [`ReplayRefusal::LedgerFailed`] when the write fails.
    pub fn reply_timestamp(
        &mut self,
        admitted: Admitted,
        valid_until: TierDeadlines,
        timestamp_key: &dyn Signer,
        delegation: SignedDelegation,
        hub_ms: HubMs,
    ) -> Result<SignedTimestamp, ReplayRefusal> {
        let held = &admitted.ledger;
        let Some(snapshot_hash) = held.acked_hash else {
            return Err(ReplayRefusal::ReplyRefused);
        };
        if admitted.kind != RequestKind::Snapshot {
            return Err(ReplayRefusal::ReplyRefused);
        }
        let (nonce, next) = next_issue(held, hub_ms)?;
        let signed = sign_timestamp(
            Timestamp {
                tenant: self.tenant.clone(),
                node: admitted.node.clone(),
                epoch: held.acked_epoch,
                snapshot_hash,
                signed_at: hub_ms,
                valid_until,
                nonce,
            },
            timestamp_key,
            delegation,
        );
        self.commit(&admitted, next)?;
        Ok(signed)
    }

    /// Re-enrols a flagged node: the operator `by` has decided the node is genuine again (a
    /// reinstall, a replaced disk). The decision is audited first and not taken if the audit
    /// fails.
    ///
    /// The acknowledged epoch, hash and the node's reports are forgotten (a fresh node holds
    /// none); every nonce and hub-time high-water mark is kept, so nothing issued before can
    /// be replayed as new. Nothing is deleted from the audit trail.
    ///
    /// # Errors
    ///
    /// [`ReplayRefusal::AuditFailed`] or [`ReplayRefusal::LedgerFailed`].
    pub fn reenrol(
        &mut self,
        node: &NodeId,
        by: &str,
        at: OffsetDateTime,
    ) -> Result<(), ReplayRefusal> {
        let ledger = self
            .ledger
            .get(node)
            .map_err(|_| ReplayRefusal::LedgerFailed)?;
        let record = record(
            KIND_NODE_REENROLLED,
            actor(ACTOR_USER, by),
            at,
            [
                ("node", PayloadValue::Text(node.as_str().to_owned())),
                ("acknowledged_epoch", number(ledger.acked_epoch)),
                (
                    "acknowledged_hash",
                    PayloadValue::Text(hex(ledger.acked_hash)),
                ),
                ("issued_nonce", number(ledger.issued_nonce)),
                (
                    "was_flagged",
                    PayloadValue::Bool(ledger.requires_reenrolment),
                ),
                ("enrolment", number(ledger.enrolment.saturating_add(1))),
            ],
        );
        self.audit
            .record(record)
            .map_err(|_| ReplayRefusal::AuditFailed)?;
        let next = NodeLedger {
            acked_epoch: 0,
            acked_hash: None,
            reported_nonce: 0,
            reported_attested: 0,
            requires_reenrolment: false,
            enrolment: ledger.enrolment.saturating_add(1),
            ..ledger
        };
        self.ledger
            .replace(node, &ledger, next)
            .map_err(|_| ReplayRefusal::LedgerFailed)
    }

    /// Writes the reply's ledger entry. The reply is built already; it is returned only if this
    /// succeeds.
    fn commit(&self, admitted: &Admitted, next: NodeLedger) -> Result<(), ReplayRefusal> {
        self.ledger
            .replace(&admitted.node, &admitted.ledger, next)
            .map_err(|_| ReplayRefusal::LedgerFailed)
    }

    fn audit_refusal(
        &mut self,
        kind: &str,
        node: &NodeId,
        ledger: &NodeLedger,
        report: &NodeReport,
        at: OffsetDateTime,
    ) {
        let record = record(
            kind,
            actor(ACTOR_NODE, node.as_str()),
            at,
            [
                ("node", PayloadValue::Text(node.as_str().to_owned())),
                ("acknowledged_epoch", number(ledger.acked_epoch)),
                ("reported_epoch", number(report.highest_epoch)),
                (
                    "acknowledged_hash",
                    PayloadValue::Text(hex(ledger.acked_hash)),
                ),
                (
                    "reported_hash",
                    PayloadValue::Text(hex(report.snapshot_hash)),
                ),
                ("issued_nonce", number(ledger.issued_nonce)),
                ("reported_nonce", number(report.hub_nonce)),
                ("known_reported_nonce", number(ledger.reported_nonce)),
                ("issued_hub_ms", number(ledger.issued_hub_ms)),
                ("reported_attested", number(report.last_attested)),
                ("known_reported_attested", number(ledger.reported_attested)),
            ],
        );
        if self.audit.record(record).is_err() {
            self.audit_failures = self.audit_failures.saturating_add(1);
        }
    }
}

enum Verdict {
    Accept,
    Regressed,
    Ahead,
}

/// Compares what the node says it holds with what the hub knows.
///
/// Regression wins over ahead. The epoch and hash are compared with what the hub acknowledged;
/// the nonce and clock with what the node itself reported before, so a reply lost on the way
/// back is not mistaken for a replay (the node still holds an older nonce than the hub issued,
/// but not an older one than it reported).
fn judge(ledger: &NodeLedger, report: &NodeReport) -> Verdict {
    let regressed = report.highest_epoch < ledger.acked_epoch
        || (report.highest_epoch == ledger.acked_epoch
            && report.snapshot_hash != ledger.acked_hash)
        || report.hub_nonce < ledger.reported_nonce
        || report.last_attested < ledger.reported_attested;
    if regressed {
        return Verdict::Regressed;
    }
    let ahead = report.highest_epoch > ledger.acked_epoch
        || report.hub_nonce > ledger.issued_nonce
        || report.last_attested > ledger.issued_hub_ms;
    if ahead {
        Verdict::Ahead
    } else {
        Verdict::Accept
    }
}

/// The nonce and ledger of the next reply: strictly above every nonce issued, with hub time
/// that never goes back. Checked arithmetic; an exhausted counter is a refusal.
fn next_issue(ledger: &NodeLedger, hub_ms: HubMs) -> Result<(u64, NodeLedger), ReplayRefusal> {
    let nonce = ledger
        .issued_nonce
        .checked_add(1)
        .ok_or(ReplayRefusal::ReplyRefused)?;
    if hub_ms <= ledger.issued_hub_ms {
        return Err(ReplayRefusal::ReplyRefused);
    }
    Ok((
        nonce,
        NodeLedger {
            issued_nonce: nonce,
            issued_hub_ms: hub_ms,
            ..*ledger
        },
    ))
}

/// A [`StandingPort`] that also refuses a node flagged for re-enrolment.
///
/// Wrap the live registry standing with it and the ledger the guard writes: a node that
/// reported a regression gets no number range either, until an operator re-enrols it. An
/// unreadable ledger is [`StandingUnavailable`].
pub struct ReenrolmentStanding<S, L> {
    inner: S,
    ledger: L,
}

impl<S, L> ReenrolmentStanding<S, L> {
    /// `inner` standing, refined by the flags in `ledger`.
    pub fn new(inner: S, ledger: L) -> Self {
        Self { inner, ledger }
    }

    /// The wrapped standing port.
    pub fn inner_mut(&mut self) -> &mut S {
        &mut self.inner
    }
}

impl<S: StandingPort, L: LedgerStore> StandingPort for ReenrolmentStanding<S, L> {
    fn standing(&self, node: &spot_sync_protocol::NodeId) -> Result<Standing, StandingUnavailable> {
        let standing = self.inner.standing(node)?;
        if standing != Standing::Active {
            return Ok(standing);
        }
        let Ok(id) = NodeId::new(node.as_str()) else {
            return Ok(Standing::Unknown);
        };
        let ledger = self.ledger.get(&id).map_err(|_| StandingUnavailable)?;
        Ok(if ledger.requires_reenrolment {
            Standing::ReenrolmentRequired
        } else {
            Standing::Active
        })
    }
}

fn record<const N: usize>(
    kind: &str,
    actor: String,
    at: OffsetDateTime,
    fields: [(&str, PayloadValue); N],
) -> AuditRecord {
    AuditRecord {
        kind: kind.to_owned(),
        actor,
        on_behalf_of: None,
        subject: None,
        at: i64::try_from(at.unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX),
        payload: fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        masked: true,
    }
}

/// A number for the audit payload: the value while it fits, else its exact decimal text.
fn number(v: u64) -> PayloadValue {
    i64::try_from(v).map_or_else(|_| PayloadValue::Text(v.to_string()), PayloadValue::Int)
}

/// The public SHA-256 of a snapshot as lower-case hex, or empty when there is none.
fn hex(hash: Option<SnapshotHash>) -> String {
    let mut out = String::new();
    if let Some(hash) = hash {
        for b in hash {
            let _ = write!(out, "{b:02x}");
        }
    }
    out
}
