//! [`PgEnrolmentStore`]: the durable, transactional, per-tenant enrolment store on PostgreSQL.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_sync_enrolment` | one row per enrolment: id, node, role, partitions, template, the code's **salted hash and salt** (never the code), expiry, `used_at`, creator |
//! | `spot_sync_enrol_node` | one row per node: role, status (`active`, `suspended`, `revoked`), partitions |
//! | `spot_sync_node_cert` | one row per pinned certificate: fingerprint, node, state (`active`, `overlap`, `retired`, `revoked`), DER, validity, `retired_at` |
//! | `spot_sync_enrol_attempt` | failed redemptions per source: count, window start, lockout end |
//!
//! Every key starts with the tenant and every statement carries the tenant predicate
//! ([`STATEMENTS`]). Every change is **one transaction with its `sys_sync_node_admin` outbox
//! record** (an insert into the tenant's own `spot_outbox`): when the outbox insert fails, the
//! enrolment, rotation, status change or revocation does not exist. Redemption locks the
//! enrolment row (`FOR UPDATE`), so of any number of concurrent redemptions of one code exactly
//! one wins; a failed redemption is counted in its own committed transaction, so the refusal
//! does not roll the count back.
//!
//! The traits are synchronous, so every call crosses the shared blocking bridge: use the store
//! from the hub's control-plane process, never from an async task (ADR-082 section 2).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use spot_pg_bridge::{Bridge, BridgeConfig, BridgeError, PooledConn};
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::code::verify_secret;
use crate::error::{EnrolError, Result};
use crate::model::{
    AUDIT_TOPIC, AuditEvent, CertState, EXPIRY_AMBER_SECONDS, ExpiringCert, LOCKOUT_SECONDS,
    MAX_FAILURES, NewEnrolment, NodeStatus, NodeSummary, PinnedCert, RedeemRequest, Redeemed,
    Rotation, clamp_overlap, effective_state, severity,
};
use crate::store::EnrolmentStore;

/// Deadline of a write.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of a read.
pub const READ_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of the table creation.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

/// The tables, in creation order.
pub const TABLES: [&str; 4] = [
    "spot_sync_enrolment",
    "spot_sync_enrol_node",
    "spot_sync_node_cert",
    "spot_sync_enrol_attempt",
];

const PREPARE_LOCK: i64 = 0x0053_594e_4345_4e01;

/// The idempotent DDL.
pub const DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_sync_enrolment (\
    tenant text NOT NULL, id text NOT NULL, node_id text NOT NULL, role text NOT NULL, \
    partitions text NOT NULL, template text NOT NULL, salt bytea NOT NULL, hash bytea NOT NULL, \
    expires_at bigint NOT NULL, used_at bigint, created_by text NOT NULL, created_at bigint NOT NULL, \
    PRIMARY KEY (tenant, id), \
    CHECK (octet_length(salt) = 16 AND octet_length(hash) = 32)); \
CREATE TABLE IF NOT EXISTS spot_sync_enrol_node (\
    tenant text NOT NULL, node_id text NOT NULL, role text NOT NULL, \
    status text NOT NULL CHECK (status IN ('active', 'suspended', 'revoked')), \
    partitions text NOT NULL, updated_at bigint NOT NULL, \
    PRIMARY KEY (tenant, node_id)); \
CREATE TABLE IF NOT EXISTS spot_sync_node_cert (\
    tenant text NOT NULL, fingerprint text NOT NULL, node_id text NOT NULL, \
    state text NOT NULL CHECK (state IN ('active', 'overlap', 'retired', 'revoked')), \
    der bytea NOT NULL, not_before bigint NOT NULL, not_after bigint NOT NULL, \
    pinned_at bigint NOT NULL, retired_at bigint, \
    PRIMARY KEY (tenant, fingerprint)); \
CREATE INDEX IF NOT EXISTS spot_sync_node_cert_node ON spot_sync_node_cert (tenant, node_id, state); \
CREATE TABLE IF NOT EXISTS spot_sync_enrol_attempt (\
    tenant text NOT NULL, source text NOT NULL, failures integer NOT NULL, \
    window_start bigint NOT NULL, locked_until bigint NOT NULL, \
    PRIMARY KEY (tenant, source));";

const NODE_STATUS: &str =
    "SELECT status FROM spot_sync_enrol_node WHERE tenant = $1 AND node_id = $2";
const NODE_STATUS_LOCK: &str = "SELECT status FROM spot_sync_enrol_node \
    WHERE tenant = $1 AND node_id = $2 FOR UPDATE";
const ENROL_INSERT: &str = "INSERT INTO spot_sync_enrolment \
    (tenant, id, node_id, role, partitions, template, salt, hash, expires_at, created_by, created_at) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT (tenant, id) DO NOTHING";
const LOCKOUT: &str =
    "SELECT locked_until FROM spot_sync_enrol_attempt WHERE tenant = $1 AND source = $2";
const ENROL_LOCK: &str = "SELECT node_id, role, partitions, template, salt, hash, expires_at, used_at \
    FROM spot_sync_enrolment WHERE tenant = $1 AND id = $2 FOR UPDATE";
const ENROL_USE: &str = "UPDATE spot_sync_enrolment SET used_at = $3 \
    WHERE tenant = $1 AND id = $2 AND used_at IS NULL";
const CERT_STATE: &str =
    "SELECT state FROM spot_sync_node_cert WHERE tenant = $1 AND fingerprint = $2";
const NODE_UPSERT: &str = "INSERT INTO spot_sync_enrol_node (tenant, node_id, role, status, partitions, updated_at) \
    VALUES ($1, $2, $3, 'active', $4, $5) ON CONFLICT (tenant, node_id) DO UPDATE SET \
    role = EXCLUDED.role, status = 'active', partitions = EXCLUDED.partitions, updated_at = EXCLUDED.updated_at";
const CERT_INSERT: &str = "INSERT INTO spot_sync_node_cert \
    (tenant, fingerprint, node_id, state, der, not_before, not_after, pinned_at) \
    VALUES ($1, $2, $3, 'active', $4, $5, $6, $7)";
const FAILURE_COUNT: &str = "INSERT INTO spot_sync_enrol_attempt (tenant, source, failures, window_start, locked_until) \
    VALUES ($1, $2, 1, $3, 0) ON CONFLICT (tenant, source) DO UPDATE SET \
    failures = CASE WHEN $3 - spot_sync_enrol_attempt.window_start >= 3600 THEN 1 \
                    ELSE spot_sync_enrol_attempt.failures + 1 END, \
    window_start = CASE WHEN $3 - spot_sync_enrol_attempt.window_start >= 3600 THEN $3 \
                        ELSE spot_sync_enrol_attempt.window_start END";
const FAILURE_LOCK: &str = "UPDATE spot_sync_enrol_attempt SET locked_until = $3::bigint + 3600, failures = 0, window_start = $3::bigint \
    WHERE tenant = $1 AND source = $2 AND failures >= $4";
const CERTS_RETIRE_OVERLAP: &str = "UPDATE spot_sync_node_cert SET state = 'retired', retired_at = $3 \
    WHERE tenant = $1 AND node_id = $2 AND state = 'overlap'";
const CERTS_ACTIVE_TO_OVERLAP: &str = "UPDATE spot_sync_node_cert SET state = 'overlap', retired_at = $3 \
    WHERE tenant = $1 AND node_id = $2 AND state = 'active'";
const NODE_SET: &str = "UPDATE spot_sync_enrol_node SET status = $3, updated_at = $4 \
    WHERE tenant = $1 AND node_id = $2";
const CERTS_REVOKE: &str = "UPDATE spot_sync_node_cert SET state = 'revoked', retired_at = NULL \
    WHERE tenant = $1 AND node_id = $2";
const LIST: &str = "SELECT n.node_id, n.role, n.status, n.partitions, c.fingerprint, c.not_after \
    FROM spot_sync_enrol_node n LEFT JOIN spot_sync_node_cert c \
    ON c.tenant = n.tenant AND c.node_id = n.node_id AND c.state = 'active' \
    WHERE n.tenant = $1 ORDER BY n.node_id COLLATE \"C\"";
const SNAPSHOT: &str = "SELECT c.fingerprint, c.node_id, n.partitions, n.status, c.state, c.retired_at, \
    c.not_before, c.not_after FROM spot_sync_node_cert c JOIN spot_sync_enrol_node n \
    ON n.tenant = c.tenant AND n.node_id = c.node_id \
    WHERE c.tenant = $1 AND c.state <> 'retired' ORDER BY c.fingerprint";
const ACTIVE_DER: &str = "SELECT der FROM spot_sync_node_cert \
    WHERE tenant = $1 AND node_id = $2 AND state = 'active'";
const EXPIRING: &str = "SELECT c.fingerprint, c.node_id, c.not_after FROM spot_sync_node_cert c \
    JOIN spot_sync_enrol_node n ON n.tenant = c.tenant AND n.node_id = c.node_id \
    WHERE c.tenant = $1 AND c.state = 'active' AND n.status = 'active' AND c.not_after <= $2 \
    ORDER BY c.not_after, c.fingerprint";
/// The audit record, written into the tenant's own outbox in the same transaction.
const OUTBOX: &str = "INSERT INTO spot_outbox (id, tx, ordinal, topic, subject_entity, subject_id, \
    subject_version, schema_version, payload, captured_at, xid) \
    VALUES ($1::text::uuid, $2::text::uuid, 0, $3, 'sync_node', $4::text::uuid, NULL, 1, $5, \
    to_timestamp($6::bigint::double precision), pg_current_xact_id())";

/// Every statement this store runs on its own tables, so a test can prove each has the tenant
/// predicate (an INSERT names the tenant column).
pub const STATEMENTS: &[&str] = &[
    NODE_STATUS,
    NODE_STATUS_LOCK,
    ENROL_INSERT,
    LOCKOUT,
    ENROL_LOCK,
    ENROL_USE,
    CERT_STATE,
    NODE_UPSERT,
    CERT_INSERT,
    FAILURE_COUNT,
    FAILURE_LOCK,
    CERTS_RETIRE_OVERLAP,
    CERTS_ACTIVE_TO_OVERLAP,
    NODE_SET,
    CERTS_REVOKE,
    LIST,
    SNAPSHOT,
    ACTIVE_DER,
    EXPIRING,
];

/// What went wrong inside one bridged operation. Never shown.
enum Op {
    Db,
    Decode,
    Refused(EnrolError),
}

impl From<tokio_postgres::Error> for Op {
    fn from(_: tokio_postgres::Error) -> Self {
        Self::Db
    }
}

fn map(error: BridgeError<Op>) -> EnrolError {
    match error {
        BridgeError::Store(Op::Refused(inner)) => inner,
        BridgeError::Store(Op::Decode) => EnrolError::StoreFailed {
            reason: "a stored enrolment row could not be read",
        },
        BridgeError::Store(Op::Db) => EnrolError::StoreFailed {
            reason: "the enrolment store query failed",
        },
        BridgeError::WrongThread => EnrolError::StoreFailed {
            reason: "pg_bridge.wrong_thread: called inside an async runtime",
        },
        BridgeError::Busy => EnrolError::StoreFailed {
            reason: "pg_bridge.busy: the enrolment store queue is full",
        },
        BridgeError::Timeout => EnrolError::StoreFailed {
            reason: "pg_bridge.timeout: the call missed its deadline; treat it as not done",
        },
        BridgeError::Closed => EnrolError::StoreFailed {
            reason: "pg_bridge.closed: the bridge is shut down",
        },
        BridgeError::Connect => EnrolError::StoreFailed {
            reason: "pg_bridge.connect: no database connection",
        },
        _ => EnrolError::StoreFailed {
            reason: "the enrolment store bridge refused the call",
        },
    }
}

fn refused(error: EnrolError) -> Op {
    Op::Refused(error)
}

/// The enrolment store of one tenant on PostgreSQL.
#[derive(Clone)]
pub struct PgEnrolmentStore {
    bridge: Arc<Bridge>,
    tenant: String,
}

impl std::fmt::Debug for PgEnrolmentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgEnrolmentStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

fn partitions_text(partitions: &[String]) -> std::result::Result<String, Op> {
    serde_json::to_string(partitions).map_err(|_| Op::Decode)
}

fn partitions_of(text: &str) -> std::result::Result<Vec<String>, Op> {
    serde_json::from_str(text).map_err(|_| Op::Decode)
}

fn node_of(text: &str) -> std::result::Result<NodeId, Op> {
    NodeId::new(text).map_err(|_| Op::Decode)
}

/// Writes the audit record into the tenant's outbox inside `tx`.
async fn write_audit(
    tx: &tokio_postgres::Transaction<'_>,
    audit: &AuditEvent,
) -> std::result::Result<(), Op> {
    let occurred = OffsetDateTime::from_unix_timestamp(audit.at)
        .map_err(|_| Op::Decode)?
        .format(&Rfc3339)
        .map_err(|_| Op::Decode)?;
    let payload = serde_json::json!({
        "meta": {
            "occurred_at": occurred,
            "actor": {"kind": "system", "component": "sync_enrol"},
            "correlation_id": audit.subject_id,
        },
        "data": {
            "action": audit.action,
            "node": audit.node.as_str(),
            "enrolment": audit.enrolment_id,
            "fingerprint": audit.fingerprint,
            "by": audit.actor,
        },
    })
    .to_string();
    tx.execute(
        OUTBOX,
        &[
            &audit.event_id,
            &audit.transaction_id,
            &AUDIT_TOPIC,
            &audit.subject_id,
            &payload,
            &audit.at,
        ],
    )
    .await?;
    Ok(())
}

impl PgEnrolmentStore {
    /// A store on `bridge` for `tenant`; `pool_size` is the size of the pool the bridge was built
    /// on. No I/O happens here.
    ///
    /// # Errors
    /// `sync_enrol.unsupported_context` on an async runtime thread, a bridge wider than the
    /// pool, or an empty or over-long tenant.
    pub fn new(bridge: Arc<Bridge>, pool_size: usize, tenant: impl Into<String>) -> Result<Self> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(EnrolError::UnsupportedContext {
                reason: "the store is built inside an async runtime; build it on a plain thread",
            });
        }
        if bridge.config().max_in_flight > BridgeConfig::largest_in_flight(pool_size) {
            return Err(EnrolError::UnsupportedContext {
                reason: "the bridge allows more calls in flight than the pool can serve",
            });
        }
        let tenant = tenant.into();
        if tenant.is_empty() || tenant.len() > 128 || tenant.chars().any(char::is_control) {
            return Err(EnrolError::UnsupportedContext {
                reason: "the tenant is empty, too long or not printable",
            });
        }
        Ok(Self { bridge, tenant })
    }

    /// Creates the tables (idempotent).
    ///
    /// # Errors
    /// `sync_enrol.prepare_failed`.
    pub fn prepare(&self) -> Result<()> {
        self.bridge
            .run(PREPARE_DEADLINE, |mut conn: PooledConn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
                        .await?;
                    tx.batch_execute(DDL).await?;
                    tx.commit().await?;
                    Ok::<(), Op>(())
                })
            })
            .map_err(|_| EnrolError::PrepareFailed)
    }

    fn run<T, F>(&self, deadline: Duration, op: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(
                PooledConn,
                String,
            ) -> spot_pg_bridge::BoxFuture<'static, std::result::Result<T, Op>>
            + Send
            + 'static,
    {
        let tenant = self.tenant.clone();
        self.bridge
            .run(deadline, move |conn| op(conn, tenant))
            .map_err(map)
    }
}

impl EnrolmentStore for PgEnrolmentStore {
    fn create_enrolment(&self, new: &NewEnrolment, audit: &AuditEvent) -> Result<()> {
        let (new, audit) = (new.clone(), audit.clone());
        self.run(WRITE_DEADLINE, move |mut conn, tenant| {
            Box::pin(async move {
                let node = new.node.as_str().to_owned();
                let partitions = partitions_text(&new.partitions)?;
                let tx = conn.transaction().await?;
                if let Some(row) = tx.query_opt(NODE_STATUS, &[&tenant, &node]).await? {
                    let status: String = row.get(0);
                    if status != "revoked" {
                        return Err(refused(EnrolError::NodeExists));
                    }
                }
                let salt: &[u8] = &new.salt;
                let hash: &[u8] = &new.hash;
                let inserted = tx
                    .execute(
                        ENROL_INSERT,
                        &[
                            &tenant,
                            &new.id,
                            &node,
                            &new.role,
                            &partitions,
                            &new.template,
                            &salt,
                            &hash,
                            &new.expires_at,
                            &new.created_by,
                            &new.created_at,
                        ],
                    )
                    .await?;
                if inserted != 1 {
                    return Err(refused(EnrolError::InvalidArgument {
                        reason: "the enrolment id is already used",
                    }));
                }
                write_audit(&tx, &audit).await?;
                tx.commit().await?;
                Ok(())
            })
        })
    }

    fn redeem(&self, request: &RedeemRequest<'_>, audit: &AuditEvent) -> Result<Redeemed> {
        let (id, secret, node) = (
            request.id.to_owned(),
            request.secret.to_vec(),
            request.node.clone(),
        );
        let (source, now) = (request.source.to_owned(), request.now);
        let (der, fingerprint, not_before, not_after) = (
            request.cert.der().to_vec(),
            request.cert.fingerprint().to_owned(),
            request.cert.not_before(),
            request.cert.not_after(),
        );
        let audit = audit.clone();
        self.run(WRITE_DEADLINE, move |mut conn, tenant| {
            Box::pin(async move {
                // Locked out: no hash work, no row read.
                if let Some(row) = conn.query_opt(LOCKOUT, &[&tenant, &source]).await? {
                    let until: i64 = row.get(0);
                    if until > now {
                        return Err(refused(EnrolError::LockedOut));
                    }
                }
                let node_text = node.as_str().to_owned();
                let tx = conn.transaction().await?;
                let row = tx.query_opt(ENROL_LOCK, &[&tenant, &id]).await?;
                // The secret is always checked (against a dummy for an unknown id), so the work
                // and the answer do not depend on which part of the code is wrong.
                let (salt, hash) = match &row {
                    Some(row) => (row.get::<_, Vec<u8>>(4), row.get::<_, Vec<u8>>(5)),
                    None => (vec![0u8; 16], vec![0u8; 32]),
                };
                let secret_ok = verify_secret(&salt, &hash, &secret);
                let usable = row.as_ref().is_some_and(|row| {
                    row.get::<_, String>(0) == node_text
                        && row.get::<_, i64>(6) > now
                        && row.get::<_, Option<i64>>(7).is_none()
                });
                if !(secret_ok && usable) {
                    tx.rollback().await?;
                    // The failure is counted in its own committed transaction.
                    let tx = conn.transaction().await?;
                    tx.execute(FAILURE_COUNT, &[&tenant, &source, &now]).await?;
                    tx.execute(
                        FAILURE_LOCK,
                        &[
                            &tenant,
                            &source,
                            &now,
                            &i32::try_from(MAX_FAILURES).unwrap_or(5),
                        ],
                    )
                    .await?;
                    tx.commit().await?;
                    return Err(refused(EnrolError::CodeInvalid));
                }
                let Some(row) = row else {
                    return Err(refused(EnrolError::CodeInvalid));
                };
                let role: String = row.get(1);
                let partitions_json: String = row.get(2);
                let template: String = row.get(3);
                // The code is good: refusals from here do not consume it.
                if let Some(existing) = tx.query_opt(NODE_STATUS, &[&tenant, &node_text]).await? {
                    let status: String = existing.get(0);
                    if status != "revoked" {
                        return Err(refused(EnrolError::NodeExists));
                    }
                }
                if let Some(existing) = tx.query_opt(CERT_STATE, &[&tenant, &fingerprint]).await? {
                    let state: String = existing.get(0);
                    return Err(refused(if state == "revoked" {
                        EnrolError::CertRevoked
                    } else {
                        EnrolError::CertInvalid
                    }));
                }
                let used = tx.execute(ENROL_USE, &[&tenant, &id, &now]).await?;
                if used != 1 {
                    return Err(refused(EnrolError::CodeInvalid));
                }
                tx.execute(
                    NODE_UPSERT,
                    &[&tenant, &node_text, &role, &partitions_json, &now],
                )
                .await?;
                let der_ref: &[u8] = &der;
                tx.execute(
                    CERT_INSERT,
                    &[
                        &tenant,
                        &fingerprint,
                        &node_text,
                        &der_ref,
                        &not_before,
                        &not_after,
                        &now,
                    ],
                )
                .await?;
                write_audit(&tx, &audit).await?;
                tx.commit().await?;
                Ok(Redeemed {
                    node,
                    partitions: partitions_of(&partitions_json)?,
                    role,
                    template,
                    fingerprint,
                })
            })
        })
    }

    fn rotate(&self, rotation: &Rotation<'_>, audit: &AuditEvent) -> Result<()> {
        let node = rotation.node.as_str().to_owned();
        let (der, fingerprint, not_before, not_after) = (
            rotation.new_cert.der().to_vec(),
            rotation.new_cert.fingerprint().to_owned(),
            rotation.new_cert.not_before(),
            rotation.new_cert.not_after(),
        );
        let now = rotation.now;
        let end = now + clamp_overlap(rotation.overlap_seconds);
        let audit = audit.clone();
        self.run(WRITE_DEADLINE, move |mut conn, tenant| {
            Box::pin(async move {
                let tx = conn.transaction().await?;
                match tx.query_opt(NODE_STATUS_LOCK, &[&tenant, &node]).await? {
                    None => return Err(refused(EnrolError::NodeUnknown)),
                    Some(row) if row.get::<_, String>(0) == "revoked" => {
                        return Err(refused(EnrolError::NodeRevoked));
                    }
                    Some(_) => {}
                }
                if let Some(existing) = tx.query_opt(CERT_STATE, &[&tenant, &fingerprint]).await? {
                    let state: String = existing.get(0);
                    return Err(refused(if state == "revoked" {
                        EnrolError::CertRevoked
                    } else {
                        EnrolError::CertInvalid
                    }));
                }
                tx.execute(CERTS_RETIRE_OVERLAP, &[&tenant, &node, &now])
                    .await?;
                tx.execute(CERTS_ACTIVE_TO_OVERLAP, &[&tenant, &node, &end])
                    .await?;
                let der_ref: &[u8] = &der;
                tx.execute(
                    CERT_INSERT,
                    &[
                        &tenant,
                        &fingerprint,
                        &node,
                        &der_ref,
                        &not_before,
                        &not_after,
                        &now,
                    ],
                )
                .await?;
                write_audit(&tx, &audit).await?;
                tx.commit().await?;
                Ok(())
            })
        })
    }

    fn set_status(
        &self,
        node: &NodeId,
        to: NodeStatus,
        now: i64,
        audit: &AuditEvent,
    ) -> Result<()> {
        let node = node.as_str().to_owned();
        let audit = audit.clone();
        self.run(WRITE_DEADLINE, move |mut conn, tenant| {
            Box::pin(async move {
                let tx = conn.transaction().await?;
                match tx.query_opt(NODE_STATUS_LOCK, &[&tenant, &node]).await? {
                    None => return Err(refused(EnrolError::NodeUnknown)),
                    Some(row) if row.get::<_, String>(0) == "revoked" => {
                        return Err(refused(EnrolError::NodeRevoked));
                    }
                    Some(_) => {}
                }
                let status = to.as_str();
                tx.execute(NODE_SET, &[&tenant, &node, &status, &now])
                    .await?;
                if to == NodeStatus::Revoked {
                    tx.execute(CERTS_REVOKE, &[&tenant, &node]).await?;
                }
                write_audit(&tx, &audit).await?;
                tx.commit().await?;
                Ok(())
            })
        })
    }

    fn list(&self) -> Result<Vec<NodeSummary>> {
        self.run(READ_DEADLINE, move |conn, tenant| {
            Box::pin(async move {
                let rows = conn.query(LIST, &[&tenant]).await?;
                rows.iter()
                    .map(|row| {
                        Ok(NodeSummary {
                            node: node_of(&row.get::<_, String>(0))?,
                            role: row.get(1),
                            status: NodeStatus::parse(&row.get::<_, String>(2))
                                .ok_or(Op::Decode)?,
                            partitions: partitions_of(&row.get::<_, String>(3))?,
                            fingerprint: row.get(4),
                            not_after: row.get(5),
                        })
                    })
                    .collect()
            })
        })
    }

    fn snapshot(&self, now: i64) -> Result<Vec<PinnedCert>> {
        self.run(READ_DEADLINE, move |conn, tenant| {
            Box::pin(async move {
                let rows = conn.query(SNAPSHOT, &[&tenant]).await?;
                let mut out = Vec::with_capacity(rows.len());
                for row in &rows {
                    let stored = CertState::parse(&row.get::<_, String>(4)).ok_or(Op::Decode)?;
                    let state = effective_state(stored, row.get::<_, Option<i64>>(5), now);
                    if state == CertState::Retired {
                        continue;
                    }
                    out.push(PinnedCert {
                        fingerprint: row.get(0),
                        node: node_of(&row.get::<_, String>(1))?,
                        partitions: partitions_of(&row.get::<_, String>(2))?
                            .into_iter()
                            .collect::<BTreeSet<_>>(),
                        node_status: NodeStatus::parse(&row.get::<_, String>(3))
                            .ok_or(Op::Decode)?,
                        cert_state: state,
                        not_before: row.get(6),
                        not_after: row.get(7),
                    });
                }
                Ok(out)
            })
        })
    }

    fn active_certificate(&self, node: &NodeId) -> Result<Vec<u8>> {
        let node = node.as_str().to_owned();
        self.run(READ_DEADLINE, move |conn, tenant| {
            Box::pin(async move {
                match conn.query_opt(ACTIVE_DER, &[&tenant, &node]).await? {
                    Some(row) => Ok(row.get::<_, Vec<u8>>(0)),
                    None => Err(refused(EnrolError::NodeUnknown)),
                }
            })
        })
    }

    fn expiring(&self, now: i64) -> Result<Vec<ExpiringCert>> {
        self.run(READ_DEADLINE, move |conn, tenant| {
            Box::pin(async move {
                let horizon = now + EXPIRY_AMBER_SECONDS;
                let rows = conn.query(EXPIRING, &[&tenant, &horizon]).await?;
                let mut out = Vec::new();
                for row in &rows {
                    let not_after: i64 = row.get(2);
                    if let Some(severity) = severity(not_after, now) {
                        out.push(ExpiringCert {
                            node: node_of(&row.get::<_, String>(1))?,
                            fingerprint: row.get(0),
                            not_after,
                            severity,
                        });
                    }
                }
                Ok(out)
            })
        })
    }
}

/// The attempts window and lockout length, for documentation and tests.
pub const LOCKOUT_WINDOW_SECONDS: i64 = LOCKOUT_SECONDS;
