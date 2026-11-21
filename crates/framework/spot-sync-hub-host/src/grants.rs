//! The hub half of the offline edge: lease requests and permission snapshot requests answered
//! (WP-RTB-03.T8; ADR-062 section 7, ADR-070 rule 12.13).
//!
//! The libraries decide; this module hosts them:
//!
//! * **Leases**: [`GrantService`] (authenticated node, standing, entitlement, size and rate caps,
//!   log before the answer, idempotent re-request by envelope id, audit) over the library's
//!   [`RebuildableAllocator`] and a **durable** grant log ([`PgGrantLog`]). A restart rebuilds
//!   the cursors, lease ids, last-grant times and request ids from the log, so no number is
//!   granted twice and a re-request after a hub crash gets the original range.
//! * **Snapshots**: [`ReplayIssuer`] over [`ReplayGuard`]: the signed node request is verified
//!   under the node's key, the report is judged against the ledger, and the answer is a signed
//!   snapshot paired with signed hub time, or a timestamp when the node already holds the
//!   current snapshot. Only the tenant's own snapshot is ever issued.
//! * **Retirement**: [`Grants::retire_node`] accounts and audits the unused part of a retired
//!   node's ranges (see the limits below).
//!
//! # The node is the connection's, never the message's
//!
//! [`CarrierHandler::handle`] takes the node the transport authenticated. Nothing in a payload
//! names the node; a payload that does not parse is ignored (no answer) and counted.
//!
//! # Tenancy
//!
//! [`Grants`] is one tenant's. [`CarrierRouter`] picks it by the authenticated node's tenant
//! (a port), so a node can reach only its own tenant's allocator, ledger and snapshot.
//!
//! # Retirement does not re-grant
//!
//! The library's allocator is strictly increasing (the rule that makes a double grant
//! impossible), and the hub cannot know which numbers a retired node used offline until it has
//! synced them. [`Grants::retire_node`] therefore returns the **unused remainder above the
//! highest number the hub has seen used** as a report and audits it; the numbers are not handed
//! out again. Re-granting them needs a release operation on the allocator (outside this task).
//!
//! # Security
//!
//! Counts, series names, numbers and codes only in answers, logs and audit records; no key,
//! signature or payload. Time is an argument. No `unsafe`, no floats.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use spot_audit_chain::{AuditRecord, PayloadValue};
use spot_bundle_signing::PublicKey;
use spot_edge_offline_host::carrier::{self, LeaseAnswerMsg, LeaseRequestMsg, SnapshotMsg};
use spot_edge_offline_host::carrier_loop::CarrierRecord;
use spot_edge_offline_host::kinds::{LEASE_ANSWER_KIND, LEASE_REQUEST_KIND, SNAPSHOT_KIND};
use spot_lease_grants::Standing;
use spot_lease_grants::{
    GrantError, GrantLog, GrantLogError, GrantPolicy, GrantRecord, GrantService, LeaseRequest,
    LedgerStore, RebuildableAllocator, ReplayGuard, ReplayRefusal, RequestId, StandingPort,
    StandingUnavailable, TransportNode,
};
use spot_offline_identity::audit::{ACTOR_NODE, actor};
use spot_offline_identity::signing::SignedNodeRequest;
use spot_offline_identity::{
    AuditSink, AuditSinkError, HubMs, Message, NodeId as IdentityNodeId, SignedDelegation,
    SignedSnapshot, Signer, TenantId, Tier, TierDeadlines,
};
use spot_partition::{LeaseId, PartitionError, RangeLease};
use spot_pg_bridge::Bridge;
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

/// The audit kind written for the unused remainder of a retired node's range.
pub const KIND_RANGE_RELEASED: &str = "lease_grants.range_released";
/// The stable code of a payload that did not parse into a request.
pub const BAD_MESSAGE: &str = "edge_offline.bad_message";

// ------------------------------------------------------------------------------------------
// The handler port
// ------------------------------------------------------------------------------------------

/// What the hub's connection layer calls with a carrier record an authenticated edge sent. The
/// answer records (zero or one) are for that node's queue.
pub trait CarrierHandler: Send + Sync {
    /// Handles `record` from `node` (the authenticated node) at `at`.
    fn handle(
        &self,
        node: &NodeId,
        record: &CarrierRecord,
        at: OffsetDateTime,
    ) -> Vec<CarrierRecord>;
}

/// Which tenant an authenticated node belongs to.
pub trait NodeTenants: Send + Sync {
    /// The tenant of `node`, or `None` when the hub does not know it.
    fn tenant_of(&self, node: &NodeId) -> Option<String>;
}

/// The public key a node's signed requests verify under.
pub trait NodeKeys: Send + Sync {
    /// The key of `node`, or `None` when none is on record.
    fn key(&self, node: &IdentityNodeId) -> Option<PublicKey>;
}

/// The hub's current signed snapshot for one node of one tenant (the IAM compile and signing is
/// not here).
pub trait SnapshotSource: Send + Sync {
    /// The current snapshot of `node` of `tenant`, or `None` when none can be issued.
    fn current(&self, tenant: &TenantId, node: &IdentityNodeId) -> Option<SignedSnapshot>;
}

/// The highest number of a series the hub has seen used by a node (from the documents it has
/// applied), for retirement.
pub trait UsedMarks: Send + Sync {
    /// The highest used number, or `None` when nothing of the series was seen used.
    fn highest_used(&self, series: &SeriesKey, node: &NodeId) -> Option<u64>;
}

// ------------------------------------------------------------------------------------------
// The two services behind object-safe traits
// ------------------------------------------------------------------------------------------

/// The lease service, as the host uses it.
pub trait LeaseGranter: Send {
    /// Grants the authenticated node a range, or refuses.
    ///
    /// # Errors
    ///
    /// [`GrantError`] with a stable `lease_grants.*` code.
    fn request(
        &mut self,
        who: &TransportNode,
        request: &LeaseRequest,
        at: OffsetDateTime,
    ) -> Result<RangeLease, GrantError>;

    /// Every grant to `node`, oldest first.
    fn grants_of(&self, node: &NodeId) -> Vec<GrantRecord>;

    /// Writes an audit record; `false` when the sink failed.
    fn audit(&mut self, record: AuditRecord) -> bool;
}

impl<A, L, S, K> LeaseGranter for GrantService<A, L, S, K>
where
    A: spot_partition::RangeAllocator + Send,
    L: GrantLog + Send,
    S: StandingPort + Send,
    K: AuditSink + Send,
{
    fn request(
        &mut self,
        who: &TransportNode,
        request: &LeaseRequest,
        at: OffsetDateTime,
    ) -> Result<RangeLease, GrantError> {
        GrantService::request(self, who, request, at)
    }

    fn grants_of(&self, node: &NodeId) -> Vec<GrantRecord> {
        self.log()
            .all()
            .into_iter()
            .filter(|record| &record.node == node)
            .collect()
    }

    fn audit(&mut self, record: AuditRecord) -> bool {
        self.audit_mut().record(record).is_ok()
    }
}

/// What a snapshot answer carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issued {
    /// The signed snapshot or timestamp.
    pub message: Message,
    /// The signed hub time paired with a snapshot.
    pub companion: Option<Message>,
}

/// The snapshot service, as the host uses it.
pub trait SnapshotIssuer: Send {
    /// Answers a signed node request, or refuses.
    ///
    /// # Errors
    ///
    /// [`ReplayRefusal`] with a stable code.
    fn issue(
        &mut self,
        who: &TransportNode,
        signed: &SignedNodeRequest,
        at: OffsetDateTime,
    ) -> Result<Issued, ReplayRefusal>;
}

/// [`SnapshotIssuer`] over a [`ReplayGuard`] of one tenant.
pub struct ReplayIssuer<L: LedgerStore, K: AuditSink> {
    tenant: TenantId,
    guard: ReplayGuard<L, K>,
    keys: Arc<dyn NodeKeys>,
    source: Arc<dyn SnapshotSource>,
    timestamp_key: Box<dyn Signer + Send>,
    delegation: SignedDelegation,
}

impl<L: LedgerStore, K: AuditSink> ReplayIssuer<L, K> {
    /// An issuer for `tenant`. The timestamp key signs hub time and timestamps; the delegation
    /// is the root's statement that it may.
    pub fn new(
        tenant: TenantId,
        guard: ReplayGuard<L, K>,
        keys: Arc<dyn NodeKeys>,
        source: Arc<dyn SnapshotSource>,
        timestamp_key: Box<dyn Signer + Send>,
        delegation: SignedDelegation,
    ) -> Self {
        Self {
            tenant,
            guard,
            keys,
            source,
            timestamp_key,
            delegation,
        }
    }
}

/// Hub milliseconds of an instant.
fn hub_ms(at: OffsetDateTime) -> HubMs {
    u64::try_from(at.unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// The deadlines a refreshed timestamp carries: the snapshot's own lease lengths from `hub_ms`.
fn refreshed(snapshot: &SignedSnapshot, hub_ms: HubMs) -> TierDeadlines {
    let body = &snapshot.snapshot;
    let length = |tier: Tier| body.valid_until.get(tier).saturating_sub(body.issued_at);
    TierDeadlines([
        hub_ms.saturating_add(length(Tier::One)),
        hub_ms.saturating_add(length(Tier::Two)),
        hub_ms.saturating_add(length(Tier::Three)),
    ])
}

impl<L: LedgerStore + Send, K: AuditSink + Send> SnapshotIssuer for ReplayIssuer<L, K> {
    fn issue(
        &mut self,
        who: &TransportNode,
        signed: &SignedNodeRequest,
        at: OffsetDateTime,
    ) -> Result<Issued, ReplayRefusal> {
        let node =
            IdentityNodeId::new(who.node().as_str()).map_err(|_| ReplayRefusal::NodeMismatch)?;
        let key = self.keys.key(&node).ok_or(ReplayRefusal::NodeMismatch)?;
        let admitted = self.guard.admit(who, &key, signed, at)?;
        let Some(current) = self.source.current(&self.tenant, &node) else {
            return Err(ReplayRefusal::ReplyRefused);
        };
        let now = hub_ms(at);
        let report = *admitted.report();
        let up_to_date = report.highest_epoch == current.snapshot.epoch
            && report.snapshot_hash == Some(current.snapshot.hash());
        if up_to_date {
            let deadlines = refreshed(&current, now);
            let timestamp = self.guard.reply_timestamp(
                admitted,
                deadlines,
                self.timestamp_key.as_ref(),
                self.delegation.clone(),
                now,
            )?;
            return Ok(Issued {
                message: Message::Timestamp(timestamp),
                companion: None,
            });
        }
        let reply =
            self.guard
                .reply_snapshot(admitted, current, self.timestamp_key.as_ref(), now)?;
        Ok(Issued {
            message: Message::Snapshot(reply.snapshot),
            companion: Some(Message::HubTime(reply.time)),
        })
    }
}

// ------------------------------------------------------------------------------------------
// The host
// ------------------------------------------------------------------------------------------

/// One range given back by a retirement report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Released {
    /// The series.
    pub series: String,
    /// First unused number.
    pub from: u64,
    /// Last number of the original grant.
    pub to: u64,
    /// The lease id of the original grant.
    pub lease_id: u64,
}

/// One tenant's grants host.
pub struct Grants {
    leases: Mutex<Box<dyn LeaseGranter>>,
    snapshots: Option<Mutex<Box<dyn SnapshotIssuer>>>,
    ignored: Mutex<u64>,
}

impl Grants {
    /// A host over the two services (`snapshots` absent: snapshot requests are refused).
    pub fn new(leases: Box<dyn LeaseGranter>, snapshots: Option<Box<dyn SnapshotIssuer>>) -> Self {
        Self {
            leases: Mutex::new(leases),
            snapshots: snapshots.map(Mutex::new),
            ignored: Mutex::new(0),
        }
    }

    /// How many payloads were ignored because they did not parse.
    pub fn ignored(&self) -> u64 {
        *self.ignored.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn ignore(&self) {
        let mut count = self.ignored.lock().unwrap_or_else(|p| p.into_inner());
        *count = count.saturating_add(1);
    }

    fn lease(
        &self,
        who: &TransportNode,
        payload: &str,
        at: OffsetDateTime,
    ) -> Option<CarrierRecord> {
        let Ok(message) = carrier::decode::<LeaseRequestMsg>(payload) else {
            self.ignore();
            return None;
        };
        let refuse = |code: &str| LeaseAnswerMsg::Refused {
            request_id: message.request_id.clone(),
            code: code.to_owned(),
        };
        let answer = match (
            SeriesKey::new(&message.series),
            RequestId::new(&message.request_id),
        ) {
            (Ok(series), Some(request_id)) => {
                let request = LeaseRequest {
                    series,
                    size: message.size,
                    claimed_node: None,
                    request_id: Some(request_id),
                };
                let outcome = self
                    .leases
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .request(who, &request, at);
                match outcome {
                    Ok(lease) => LeaseAnswerMsg::Granted {
                        request_id: message.request_id.clone(),
                        series: lease.series.as_str().to_owned(),
                        from: lease.from,
                        to: lease.to,
                        lease_id: lease.lease_id.get(),
                    },
                    Err(error) => refuse(error.code()),
                }
            }
            _ => refuse(BAD_MESSAGE),
        };
        Some(CarrierRecord {
            kind: LEASE_ANSWER_KIND.to_owned(),
            payload: carrier::encode(&answer).ok()?,
        })
    }

    fn snapshot(
        &self,
        who: &TransportNode,
        payload: &str,
        at: OffsetDateTime,
    ) -> Option<CarrierRecord> {
        let Ok(SnapshotMsg::Request { request_id, signed }) =
            carrier::decode::<SnapshotMsg>(payload)
        else {
            self.ignore();
            return None;
        };
        let outcome = match &self.snapshots {
            Some(issuer) => issuer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .issue(who, &signed, at),
            None => Err(ReplayRefusal::ReplyRefused),
        };
        let answer = match outcome {
            Ok(issued) => SnapshotMsg::Issued {
                request_id,
                message: issued.message,
                companion: issued.companion,
            },
            Err(refusal) => SnapshotMsg::Refused {
                request_id,
                code: refusal.code().to_owned(),
            },
        };
        Some(CarrierRecord {
            kind: SNAPSHOT_KIND.to_owned(),
            payload: carrier::encode(&answer).ok()?,
        })
    }

    /// The unused remainder of `node`'s ranges, accounted and audited (not re-granted: see the
    /// module notes). `marks` says how far the node's numbers were seen used.
    ///
    /// # Errors
    ///
    /// `lease_grants.audit_failed` when a record could not be written; the report is then not
    /// returned (nothing is half-accounted).
    pub fn retire_node(
        &self,
        node: &NodeId,
        marks: &dyn UsedMarks,
        at: OffsetDateTime,
    ) -> Result<Vec<Released>, &'static str> {
        let mut leases = self.leases.lock().unwrap_or_else(|p| p.into_inner());
        let mut released = Vec::new();
        for grant in leases.grants_of(node) {
            let used = marks.highest_used(&grant.lease.series, node).unwrap_or(0);
            let from = grant.lease.from.max(used.saturating_add(1));
            if from > grant.lease.to {
                continue;
            }
            released.push(Released {
                series: grant.lease.series.as_str().to_owned(),
                from,
                to: grant.lease.to,
                lease_id: grant.lease.lease_id.get(),
            });
        }
        for item in &released {
            if !leases.audit(release_record(node, item, at)) {
                return Err("lease_grants.audit_failed");
            }
        }
        Ok(released)
    }
}

fn number(value: u64) -> PayloadValue {
    match i64::try_from(value) {
        Ok(n) => PayloadValue::Int(n),
        Err(_) => PayloadValue::Text(value.to_string()),
    }
}

fn release_record(node: &NodeId, item: &Released, at: OffsetDateTime) -> AuditRecord {
    let mut payload = BTreeMap::new();
    payload.insert("series".to_owned(), PayloadValue::Text(item.series.clone()));
    payload.insert("from".to_owned(), number(item.from));
    payload.insert("to".to_owned(), number(item.to));
    payload.insert("lease_id".to_owned(), number(item.lease_id));
    AuditRecord {
        kind: KIND_RANGE_RELEASED.to_owned(),
        actor: actor(ACTOR_NODE, node.as_str()),
        on_behalf_of: None,
        subject: None,
        at: i64::try_from(at.unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX),
        payload,
        masked: true,
    }
}

impl CarrierHandler for Grants {
    fn handle(
        &self,
        node: &NodeId,
        record: &CarrierRecord,
        at: OffsetDateTime,
    ) -> Vec<CarrierRecord> {
        let who = TransportNode::authenticated(node.clone());
        let answer = match record.kind.as_str() {
            LEASE_REQUEST_KIND => self.lease(&who, &record.payload, at),
            SNAPSHOT_KIND => self.snapshot(&who, &record.payload, at),
            _ => None,
        };
        answer.into_iter().collect()
    }
}

/// Picks the tenant's [`Grants`] by the authenticated node's tenant.
pub struct CarrierRouter {
    tenants: BTreeMap<String, Arc<Grants>>,
    tenant_of: Arc<dyn NodeTenants>,
}

impl CarrierRouter {
    /// A router over `tenants` (by tenant name).
    pub fn new(tenants: BTreeMap<String, Arc<Grants>>, tenant_of: Arc<dyn NodeTenants>) -> Self {
        Self { tenants, tenant_of }
    }
}

impl CarrierHandler for CarrierRouter {
    fn handle(
        &self,
        node: &NodeId,
        record: &CarrierRecord,
        at: OffsetDateTime,
    ) -> Vec<CarrierRecord> {
        let Some(tenant) = self.tenant_of.tenant_of(node) else {
            return Vec::new();
        };
        self.tenants
            .get(&tenant)
            .map_or_else(Vec::new, |grants| grants.handle(node, record, at))
    }
}

// ------------------------------------------------------------------------------------------
// Policy from settings
// ------------------------------------------------------------------------------------------

/// One entitlement: a node may lease a series.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entitlement {
    /// The node (`<site>#<partition>`).
    pub node: String,
    /// The series key.
    pub series: String,
}

/// `[services.sync_hub_grants]`: the hub's lease policy.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PolicySettings {
    /// The largest range of a series without its own maximum.
    pub default_max: Option<u64>,
    /// The least seconds between two grants of one series to one node.
    pub min_interval_secs: Option<u64>,
    /// How many distinct series the hub tracks.
    pub max_series: Option<usize>,
    /// Per-series maximum range sizes.
    pub series_max: BTreeMap<String, u64>,
    /// Which node may lease which series (a node is entitled to nothing by default).
    pub entitle: Vec<Entitlement>,
}

impl PolicySettings {
    /// The library policy.
    ///
    /// # Errors
    ///
    /// A message for an illegal series key or node id.
    pub fn build(&self) -> Result<GrantPolicy, String> {
        let mut policy = GrantPolicy::new();
        if let Some(max) = self.default_max {
            policy = policy.with_default_max(max);
        }
        if let Some(secs) = self.min_interval_secs {
            policy = policy.with_min_interval(time::Duration::seconds(
                i64::try_from(secs).unwrap_or(i64::MAX),
            ));
        }
        if let Some(max) = self.max_series {
            policy = policy.with_max_series(max);
        }
        for (series, max) in &self.series_max {
            let key = SeriesKey::new(series)
                .map_err(|_| format!("series_max: `{series}` is not a legal series key"))?;
            policy = policy.with_series_max(&key, *max);
        }
        for entitlement in &self.entitle {
            let node = NodeId::new(&entitlement.node)
                .map_err(|_| format!("entitle: `{}` is not a legal node id", entitlement.node))?;
            let key = SeriesKey::new(&entitlement.series).map_err(|_| {
                format!(
                    "entitle: `{}` is not a legal series key",
                    entitlement.series
                )
            })?;
            policy = policy.with_node_series(&node, &key);
        }
        Ok(policy)
    }
}

// ------------------------------------------------------------------------------------------
// The ports a provider module fills, and the boxed forms of the library traits
// ------------------------------------------------------------------------------------------

/// A boxed [`GrantLog`].
pub struct DynLog(pub Box<dyn GrantLog + Send>);

impl GrantLog for DynLog {
    fn append(&mut self, record: GrantRecord) -> Result<(), GrantLogError> {
        self.0.append(record)
    }

    fn cursor(&self, series: &SeriesKey) -> Option<u64> {
        self.0.cursor(series)
    }

    fn all(&self) -> Vec<GrantRecord> {
        self.0.all()
    }
}

/// A boxed [`StandingPort`].
pub struct DynStanding(pub Box<dyn StandingPort + Send>);

impl StandingPort for DynStanding {
    fn standing(&self, node: &NodeId) -> Result<Standing, StandingUnavailable> {
        self.0.standing(node)
    }
}

/// A boxed [`AuditSink`].
pub struct DynAudit(pub Box<dyn AuditSink + Send>);

impl AuditSink for DynAudit {
    fn record(&mut self, record: AuditRecord) -> Result<(), AuditSinkError> {
        self.0.record(record)
    }
}

/// What the modules that hold the node registry, the audit chain and the snapshot signing
/// provide, for one tenant. The pieces are taken once, at start-up.
pub struct GrantsInputs {
    /// The tenant's name.
    pub tenant: String,
    /// The durable grant log.
    pub log: Mutex<Option<DynLog>>,
    /// The node standing source (a live view of the node registry).
    pub standing: Mutex<Option<DynStanding>>,
    /// The audit sink.
    pub audit: Mutex<Option<DynAudit>>,
    /// The snapshot issuer, when the hub serves snapshots.
    pub snapshots: Mutex<Option<Box<dyn SnapshotIssuer>>>,
}

impl GrantsInputs {
    /// Builds the tenant's [`Grants`] from the inputs and the policy; `None` when an input was
    /// already taken.
    pub fn open(&self, policy: GrantPolicy) -> Option<Grants> {
        let log = self.log.lock().unwrap_or_else(|p| p.into_inner()).take()?;
        let standing = self
            .standing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()?;
        let audit = self
            .audit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()?;
        let snapshots = self
            .snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let service: GrantService<RebuildableAllocator, DynLog, DynStanding, DynAudit> =
            GrantService::open(log, standing, audit, policy);
        Some(Grants::new(Box::new(service), snapshots))
    }
}

// ------------------------------------------------------------------------------------------
// The durable grant log on PostgreSQL
// ------------------------------------------------------------------------------------------

/// The grant log's idempotent DDL, one statement per object, for the migrator's extra-table hook.
pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_lease_grant_log (\
    tenant text NOT NULL, \
    series text NOT NULL, \
    lease_id bigint NOT NULL CHECK (lease_id >= 0), \
    node text NOT NULL, \
    first_number numeric(39,0) NOT NULL CHECK (first_number >= 1), \
    last_number numeric(39,0) NOT NULL, \
    requested_size numeric(39,0) NOT NULL, \
    granted_at numeric(39,0) NOT NULL, \
    request_id text, \
    PRIMARY KEY (tenant, lease_id), \
    CHECK (last_number >= first_number)); \
CREATE INDEX IF NOT EXISTS spot_lease_grant_log_series \
    ON spot_lease_grant_log (tenant, series, last_number)";

/// The table this module owns.
pub const TABLES: [&str; 1] = ["spot_lease_grant_log"];

/// Every statement that touches the table, for the tenant-predicate scan.
pub const STATEMENTS: [&str; 5] = [LOAD_ALL, CURSOR, LEASE_USED, APPEND, MISSING];

const LOAD_ALL: &str = "SELECT series, lease_id, node, first_number::text, last_number::text, \
    requested_size::text, granted_at::text, request_id FROM spot_lease_grant_log \
    WHERE tenant = $1 ORDER BY granted_at, lease_id";
const CURSOR: &str = "SELECT coalesce(max(last_number), 0)::text FROM spot_lease_grant_log \
    WHERE tenant = $1 AND series = $2";
const LEASE_USED: &str = "SELECT 1 FROM spot_lease_grant_log WHERE tenant = $1 AND lease_id = $2";
const APPEND: &str = "INSERT INTO spot_lease_grant_log (tenant, series, lease_id, node, \
    first_number, last_number, requested_size, granted_at, request_id) \
    VALUES ($1, $2, $3, $4, $5::text::numeric, $6::text::numeric, $7::text::numeric, \
    $8::text::numeric, $9)";
const MISSING: &str = "SELECT to_regclass('public.spot_lease_grant_log') IS NOT NULL";
/// Serialises the appenders of one series across processes (no table touched).
const SERIES_LOCK: &str = "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))";

/// Deadline of one bridged call.
pub const DEADLINE: Duration = Duration::from_secs(10);

type LoadedRow = (
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

enum Op {
    Db,
    Decode,
    Overlap,
}

impl From<tokio_postgres::Error> for Op {
    fn from(_error: tokio_postgres::Error) -> Self {
        Self::Db
    }
}

/// Runs a blocking call on a short-lived plain thread (the bridge refuses a runtime thread).
fn off_runtime<R: Send>(job: impl FnOnce() -> R + Send) -> Option<R> {
    std::thread::scope(|scope| scope.spawn(job).join().ok())
}

/// [`GrantLog`] over `spot_lease_grant_log`, for one tenant.
///
/// The records are read **once, at open**, and kept in memory: `all()` and `cursor()` cannot
/// report a failure, and an empty answer after a failed read would let the service start numbering
/// from 1 again. A failed read at open refuses to start instead. Every append checks the stored
/// cursor and the lease id inside one transaction, under an advisory lock of the series, so a
/// second process (or a stale cache) can only be refused with an overlap, never grant twice.
pub struct PgGrantLog {
    bridge: Arc<Bridge>,
    tenant: String,
    records: Vec<GrantRecord>,
}

impl PgGrantLog {
    /// Opens the log of `tenant`: checks the table exists and loads every record.
    ///
    /// # Errors
    ///
    /// A message for the operator (`run spot migrate` when the table is missing).
    pub fn open(bridge: Arc<Bridge>, tenant: impl Into<String>) -> Result<Self, String> {
        let tenant = tenant.into();
        let (b, t) = (Arc::clone(&bridge), tenant.clone());
        let loaded = off_runtime(move || {
            b.run(DEADLINE, move |conn| {
                Box::pin(async move {
                    let present: bool = conn.query_one(MISSING, &[]).await?.get(0);
                    if !present {
                        return Ok::<Option<Vec<LoadedRow>>, Op>(None);
                    }
                    let rows = conn.query(LOAD_ALL, &[&t]).await?;
                    Ok(Some(
                        rows.iter()
                            .map(|r| {
                                (
                                    r.get(0),
                                    r.get(1),
                                    r.get(2),
                                    r.get(3),
                                    r.get(4),
                                    r.get(5),
                                    r.get(6),
                                    r.get(7),
                                )
                            })
                            .collect(),
                    ))
                })
            })
        })
        .ok_or_else(|| "the grant log read did not finish".to_owned())?
        .map_err(|_| "the grant log could not be read".to_owned())?;
        let rows = loaded
            .ok_or_else(|| "spot_lease_grant_log is missing: run `spot migrate`".to_owned())?;
        let mut records = Vec::with_capacity(rows.len());
        for row in &rows {
            records.push(decode(row).map_err(|_| "a stored grant did not decode".to_owned())?);
        }
        Ok(Self {
            bridge,
            tenant,
            records,
        })
    }
}

fn decode(row: &LoadedRow) -> Result<GrantRecord, Op> {
    let series = SeriesKey::new(&row.0).map_err(|_| Op::Decode)?;
    let node = NodeId::new(&row.2).map_err(|_| Op::Decode)?;
    let lease_id = u64::try_from(row.1).map_err(|_| Op::Decode)?;
    let from: u64 = row.3.parse().map_err(|_| Op::Decode)?;
    let to: u64 = row.4.parse().map_err(|_| Op::Decode)?;
    let requested_size: u64 = row.5.parse().map_err(|_| Op::Decode)?;
    let nanos: i128 = row.6.parse().map_err(|_| Op::Decode)?;
    let granted_at = OffsetDateTime::from_unix_timestamp_nanos(nanos).map_err(|_| Op::Decode)?;
    let lease = RangeLease::new(series, from, to, node.clone(), LeaseId::new(lease_id))
        .map_err(|_: PartitionError| Op::Decode)?;
    Ok(GrantRecord {
        lease,
        node,
        requested_size,
        granted_at,
        request_id: row.7.as_deref().and_then(RequestId::new),
    })
}

impl GrantLog for PgGrantLog {
    fn append(&mut self, record: GrantRecord) -> Result<(), GrantLogError> {
        if self
            .cursor(&record.lease.series)
            .is_some_and(|cursor| record.lease.from <= cursor)
            || self
                .records
                .iter()
                .any(|r| r.lease.lease_id == record.lease.lease_id)
        {
            return Err(GrantLogError::Overlap);
        }
        let lease_id =
            i64::try_from(record.lease.lease_id.get()).map_err(|_| GrantLogError::Unavailable)?;
        let (bridge, tenant) = (Arc::clone(&self.bridge), self.tenant.clone());
        let series = record.lease.series.as_str().to_owned();
        let node = record.node.as_str().to_owned();
        let (from, to) = (record.lease.from.to_string(), record.lease.to.to_string());
        let size = record.requested_size.to_string();
        let at = record.granted_at.unix_timestamp_nanos().to_string();
        let request_id = record.request_id.as_ref().map(|r| r.as_str().to_owned());
        let first = record.lease.from;
        let outcome = off_runtime(move || {
            bridge.run(DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    let lock_key = format!("{tenant}:{series}");
                    tx.execute(SERIES_LOCK, &[&lock_key]).await?;
                    let stored: String = tx.query_one(CURSOR, &[&tenant, &series]).await?.get(0);
                    let stored: u128 = stored.parse().map_err(|_| Op::Decode)?;
                    let used = tx.query(LEASE_USED, &[&tenant, &lease_id]).await?;
                    if u128::from(first) <= stored || !used.is_empty() {
                        return Err(Op::Overlap);
                    }
                    tx.execute(
                        APPEND,
                        &[
                            &tenant,
                            &series,
                            &lease_id,
                            &node,
                            &from,
                            &to,
                            &size,
                            &at,
                            &request_id,
                        ],
                    )
                    .await?;
                    tx.commit().await?;
                    Ok::<(), Op>(())
                })
            })
        });
        match outcome {
            Some(Ok(())) => {
                self.records.push(record);
                Ok(())
            }
            Some(Err(spot_pg_bridge::BridgeError::Store(Op::Overlap))) => {
                Err(GrantLogError::Overlap)
            }
            _ => Err(GrantLogError::Unavailable),
        }
    }

    fn cursor(&self, series: &SeriesKey) -> Option<u64> {
        self.records
            .iter()
            .filter(|r| &r.lease.series == series)
            .map(|r| r.lease.to)
            .max()
    }

    fn all(&self) -> Vec<GrantRecord> {
        self.records.clone()
    }
}
