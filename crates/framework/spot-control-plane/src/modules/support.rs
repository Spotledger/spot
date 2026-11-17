//! The `support` module (WP-CTL-07.T2): the control-plane side of customer-approved support
//! access. The customer decides; this module records what the customer's tenant host forwards,
//! lets the operator open time-boxed sessions against a grant that exists, and tells the tenant
//! host every session so the customer sees them.
//!
//! # Who does what (overlap rule with WP-SEC-07.T2)
//!
//! The tenant side (requests, the signed consent and its verification, receipts, credentials, the
//! customer's view of actions, the two-person rule) is `spot-cli`'s `support_access` service. It
//! calls this module over the host mTLS listener when the customer approves or revokes. **This
//! module never creates, widens or extends a grant**: the only way a grant exists is a call on the
//! host listener, authenticated by the tenant host's enrolled certificate (the tenant is taken from
//! the certificate, never from the body), and there is no extend route.
//!
//! # Endpoints
//!
//! Host listener (tenant host, certificate identity; the tenant is the certificate's):
//! `POST /cp/v1/host/support-grants` (the forwarded grant: id, DocTypes, write flag, window,
//! approver and the signed record the tenant host verified, kept as evidence and never shown to
//! the operator; window at most 24 hours, not ended, not backdated), `POST .../{id}/revoke`
//! (final and immediate; closes the grant's open sessions) and `GET .../{id}/sessions` (what the
//! operator opened, for the customer-visible trail).
//!
//! Operator listener (scoped; an out-of-scope id is `cp.not_found`, like a missing one):
//! `GET /cp/v1/support-grants` (state `scheduled|active|expired|revoked`, no record, no signature),
//! `POST /cp/v1/support-grants/{tenant}/{id}/sessions` (a verified signed command; refused unless
//! the grant is active: no grant is `cp.not_found`, expired, revoked or not started is
//! `cp.forbidden`; read-only unless the grant allows writes and the request asks; at most the
//! shorter of the grant's end and eight hours; at most five open per grant),
//! `POST .../sessions/{session}/close` and `GET .../sessions`. Break-glass (WP-ADM-11.T2) is a
//! separate flow, not served here.
//!
//! # Rules
//!
//! Every outcome writes an audit record (action `support.*`, never a secret, a signature or a
//! DocType name); a session is opened only after its audit record is written. Time is the injected
//! clock. A grant is never deleted (revoked grants stay as evidence). Secrets of the customer's
//! data are not reachable through this module at all: it holds scope names and times only, and the
//! tenant host enforces scope and masking on every action.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use deadpool_postgres::Pool;
use serde::Deserialize;
use serde_json::{Value, json};
use spot_core::Clock;
use spot_mgmt_api::ports::{AuditOutcome, AuditRecord, AuditSink, BoxFuture};

use crate::auth::{HostIdentity, OperatorCall, RequestId};
use crate::{CpCode, CpCtx, CpError};

/// Registration order: after the tenant registry and the modules a session may need.
pub const ORDER: u32 = 70;

/// The control-schema DDL of this module.
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_support_grants (\
     tenant TEXT NOT NULL, id TEXT NOT NULL, doctypes TEXT NOT NULL, can_write BOOLEAN NOT NULL, \
     starts_at BIGINT NOT NULL, expires_at BIGINT NOT NULL, approver TEXT NOT NULL, \
     record TEXT NOT NULL, received_at BIGINT NOT NULL, revoked_at BIGINT, revoked_by TEXT, \
     PRIMARY KEY (tenant, id))",
    "CREATE TABLE IF NOT EXISTS cp_support_sessions (\
     tenant TEXT NOT NULL, grant_id TEXT NOT NULL, id TEXT NOT NULL, operator TEXT NOT NULL, \
     reason TEXT NOT NULL, read_only BOOLEAN NOT NULL, opened_at BIGINT NOT NULL, \
     expires_at BIGINT NOT NULL, closed_at BIGINT, command_id TEXT, \
     PRIMARY KEY (tenant, grant_id, id))",
];

const MAX_BODY: usize = 32 * 1024;
const MAX_RECORD: usize = 16 * 1024;
const MAX_SIGNATURE: usize = 1024;
const MAX_DOCTYPES: usize = 50;
const MAX_LIST: usize = 100;

/// The limits of this module.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// The longest grant window, in seconds (24 hours).
    pub max_grant_secs: i64,
    /// The longest session, in seconds (8 hours).
    pub max_session_secs: i64,
    /// Open sessions per grant.
    pub max_open_sessions: usize,
    /// How far in the past a grant may start (clock skew), in seconds.
    pub backdate_secs: i64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_grant_secs: 24 * 3600,
            max_session_secs: 8 * 3600,
            max_open_sessions: 5,
            backdate_secs: 300,
        }
    }
}

/// A grant as the control plane holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The tenant (from the host certificate).
    pub tenant: String,
    /// The grant id (the tenant's).
    pub id: String,
    /// The DocTypes in scope.
    pub doctypes: Vec<String>,
    /// Whether the customer allowed writes.
    pub can_write: bool,
    /// Window start (Unix seconds).
    pub starts_at: i64,
    /// Window end (exclusive).
    pub expires_at: i64,
    /// The approving administrator's identity.
    pub approver: String,
    /// The signed record the tenant host verified (evidence; never shown to the operator).
    pub record: String,
    /// When it arrived.
    pub received_at: i64,
    /// When the customer revoked it.
    pub revoked_at: Option<i64>,
    /// Who revoked it.
    pub revoked_by: Option<String>,
}

impl Grant {
    fn state(&self, now: i64) -> &'static str {
        if self.revoked_at.is_some() {
            "revoked"
        } else if now >= self.expires_at {
            "expired"
        } else if now < self.starts_at {
            "scheduled"
        } else {
            "active"
        }
    }
}

/// One operator session against a grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// The tenant.
    pub tenant: String,
    /// The grant.
    pub grant_id: String,
    /// The session id.
    pub id: String,
    /// The operator principal.
    pub operator: String,
    /// Why (a ticket reference or sentence the customer can read).
    pub reason: String,
    /// Whether the session is read-only.
    pub read_only: bool,
    /// When it opened.
    pub opened_at: i64,
    /// When it ends at the latest.
    pub expires_at: i64,
    /// When it was closed, if it was.
    pub closed_at: Option<i64>,
    /// The verified command that opened it.
    pub command_id: Option<String>,
}

fn store_failed() -> CpError {
    CpError::with_reason(CpCode::Internal, "support.store_failed")
}

/// The module's state. A grant is never deleted.
pub trait SupportStore: Send + Sync {
    /// Stores a new grant; `false` when the id is taken for that tenant (the first stays).
    fn insert_grant<'a>(&'a self, grant: &'a Grant) -> BoxFuture<'a, Result<bool, CpError>>;
    /// One grant of one tenant.
    fn get_grant<'a>(
        &'a self,
        tenant: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Grant>, CpError>>;
    /// Every grant (bounded), ordered by tenant and id.
    fn list_grants(&self) -> BoxFuture<'_, Result<Vec<Grant>, CpError>>;
    /// Revokes a grant and closes its open sessions at `at`; `None` for an unknown grant, the
    /// grant otherwise (an already revoked grant keeps its first revocation).
    fn revoke_grant<'a>(
        &'a self,
        tenant: &'a str,
        id: &'a str,
        by: &'a str,
        at: i64,
    ) -> BoxFuture<'a, Result<Option<Grant>, CpError>>;
    /// Opens a session; `false` when the grant already has `max_open` sessions open at `now`.
    fn open_session<'a>(
        &'a self,
        session: &'a Session,
        max_open: usize,
        now: i64,
    ) -> BoxFuture<'a, Result<bool, CpError>>;
    /// Closes a session; `false` when it is unknown or already closed.
    fn close_session<'a>(
        &'a self,
        tenant: &'a str,
        grant_id: &'a str,
        id: &'a str,
        at: i64,
    ) -> BoxFuture<'a, Result<bool, CpError>>;
    /// The sessions of a grant, newest first, at most `limit`.
    fn sessions<'a>(
        &'a self,
        tenant: &'a str,
        grant_id: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Session>, CpError>>;
}

/// The PostgreSQL store, on the control pool.
pub struct PgStore {
    pool: Pool,
}

impl PgStore {
    /// A store on the control pool (the DDL is applied by `migrate`).
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

fn grant_row(row: &tokio_postgres::Row) -> Grant {
    let doctypes: String = row.get("doctypes");
    Grant {
        tenant: row.get("tenant"),
        id: row.get("id"),
        doctypes: doctypes
            .split('\n')
            .filter(|d| !d.is_empty())
            .map(str::to_owned)
            .collect(),
        can_write: row.get("can_write"),
        starts_at: row.get("starts_at"),
        expires_at: row.get("expires_at"),
        approver: row.get("approver"),
        record: row.get("record"),
        received_at: row.get("received_at"),
        revoked_at: row.get("revoked_at"),
        revoked_by: row.get("revoked_by"),
    }
}

fn session_row(row: &tokio_postgres::Row) -> Session {
    Session {
        tenant: row.get("tenant"),
        grant_id: row.get("grant_id"),
        id: row.get("id"),
        operator: row.get("operator"),
        reason: row.get("reason"),
        read_only: row.get("read_only"),
        opened_at: row.get("opened_at"),
        expires_at: row.get("expires_at"),
        closed_at: row.get("closed_at"),
        command_id: row.get("command_id"),
    }
}

const GRANT_COLUMNS: &str = "tenant, id, doctypes, can_write, starts_at, expires_at, approver, record, received_at, revoked_at, revoked_by";
const SESSION_COLUMNS: &str = "tenant, grant_id, id, operator, reason, read_only, opened_at, expires_at, closed_at, command_id";

impl SupportStore for PgStore {
    fn insert_grant<'a>(&'a self, grant: &'a Grant) -> BoxFuture<'a, Result<bool, CpError>> {
        Box::pin(async move {
            let conn = self.pool.get().await.map_err(|_| store_failed())?;
            let doctypes = grant.doctypes.join("\n");
            let n = conn
                .execute(
                    "INSERT INTO cp_support_grants (tenant, id, doctypes, can_write, starts_at, expires_at, approver, record, received_at) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT DO NOTHING",
                    &[&grant.tenant, &grant.id, &doctypes, &grant.can_write, &grant.starts_at, &grant.expires_at, &grant.approver, &grant.record, &grant.received_at],
                )
                .await
                .map_err(|_| store_failed())?;
            Ok(n == 1)
        })
    }

    fn get_grant<'a>(
        &'a self,
        tenant: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Grant>, CpError>> {
        Box::pin(async move {
            let conn = self.pool.get().await.map_err(|_| store_failed())?;
            let row = conn
                .query_opt(
                    &format!("SELECT {GRANT_COLUMNS} FROM cp_support_grants WHERE tenant = $1 AND id = $2"),
                    &[&tenant, &id],
                )
                .await
                .map_err(|_| store_failed())?;
            Ok(row.as_ref().map(grant_row))
        })
    }

    fn list_grants(&self) -> BoxFuture<'_, Result<Vec<Grant>, CpError>> {
        Box::pin(async move {
            let conn = self.pool.get().await.map_err(|_| store_failed())?;
            let rows = conn
                .query(
                    &format!("SELECT {GRANT_COLUMNS} FROM cp_support_grants ORDER BY tenant, id LIMIT 1000"),
                    &[],
                )
                .await
                .map_err(|_| store_failed())?;
            Ok(rows.iter().map(grant_row).collect())
        })
    }

    fn revoke_grant<'a>(
        &'a self,
        tenant: &'a str,
        id: &'a str,
        by: &'a str,
        at: i64,
    ) -> BoxFuture<'a, Result<Option<Grant>, CpError>> {
        Box::pin(async move {
            let mut conn = self.pool.get().await.map_err(|_| store_failed())?;
            let tx = conn.transaction().await.map_err(|_| store_failed())?;
            tx.execute(
                "UPDATE cp_support_grants SET revoked_at = $3, revoked_by = $4 \
                 WHERE tenant = $1 AND id = $2 AND revoked_at IS NULL",
                &[&tenant, &id, &at, &by],
            )
            .await
            .map_err(|_| store_failed())?;
            tx.execute(
                "UPDATE cp_support_sessions SET closed_at = $3 \
                 WHERE tenant = $1 AND grant_id = $2 AND closed_at IS NULL",
                &[&tenant, &id, &at],
            )
            .await
            .map_err(|_| store_failed())?;
            let row = tx
                .query_opt(
                    &format!("SELECT {GRANT_COLUMNS} FROM cp_support_grants WHERE tenant = $1 AND id = $2"),
                    &[&tenant, &id],
                )
                .await
                .map_err(|_| store_failed())?;
            tx.commit().await.map_err(|_| store_failed())?;
            Ok(row.as_ref().map(grant_row))
        })
    }

    fn open_session<'a>(
        &'a self,
        session: &'a Session,
        max_open: usize,
        now: i64,
    ) -> BoxFuture<'a, Result<bool, CpError>> {
        Box::pin(async move {
            let mut conn = self.pool.get().await.map_err(|_| store_failed())?;
            let tx = conn.transaction().await.map_err(|_| store_failed())?;
            // Serialise the count and the insert per grant.
            let lock = format!("{}/{}", session.tenant, session.grant_id);
            tx.execute("SELECT pg_advisory_xact_lock(hashtext($1))", &[&lock])
                .await
                .map_err(|_| store_failed())?;
            let open: i64 = tx
                .query_one(
                    "SELECT count(*) FROM cp_support_sessions \
                     WHERE tenant = $1 AND grant_id = $2 AND closed_at IS NULL AND expires_at > $3",
                    &[&session.tenant, &session.grant_id, &now],
                )
                .await
                .map_err(|_| store_failed())?
                .get(0);
            if usize::try_from(open).unwrap_or(usize::MAX) >= max_open {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO cp_support_sessions (tenant, grant_id, id, operator, reason, read_only, opened_at, expires_at, command_id) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
                &[&session.tenant, &session.grant_id, &session.id, &session.operator, &session.reason, &session.read_only, &session.opened_at, &session.expires_at, &session.command_id],
            )
            .await
            .map_err(|_| store_failed())?;
            tx.commit().await.map_err(|_| store_failed())?;
            Ok(true)
        })
    }

    fn close_session<'a>(
        &'a self,
        tenant: &'a str,
        grant_id: &'a str,
        id: &'a str,
        at: i64,
    ) -> BoxFuture<'a, Result<bool, CpError>> {
        Box::pin(async move {
            let conn = self.pool.get().await.map_err(|_| store_failed())?;
            let n = conn
                .execute(
                    "UPDATE cp_support_sessions SET closed_at = $4 \
                     WHERE tenant = $1 AND grant_id = $2 AND id = $3 AND closed_at IS NULL",
                    &[&tenant, &grant_id, &id, &at],
                )
                .await
                .map_err(|_| store_failed())?;
            Ok(n == 1)
        })
    }

    fn sessions<'a>(
        &'a self,
        tenant: &'a str,
        grant_id: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Session>, CpError>> {
        Box::pin(async move {
            let conn = self.pool.get().await.map_err(|_| store_failed())?;
            let limit = i64::try_from(limit).unwrap_or(100);
            let rows = conn
                .query(
                    &format!("SELECT {SESSION_COLUMNS} FROM cp_support_sessions WHERE tenant = $1 AND grant_id = $2 ORDER BY opened_at DESC, id LIMIT $3"),
                    &[&tenant, &grant_id, &limit],
                )
                .await
                .map_err(|_| store_failed())?;
            Ok(rows.iter().map(session_row).collect())
        })
    }
}

/// What the tenant host forwards when the customer approves a grant.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForwardedGrant {
    id: String,
    doctypes: Vec<String>,
    #[serde(default)]
    write: bool,
    starts_at: i64,
    expires_at: i64,
    approver: String,
    record: String,
    signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenBody {
    reason: String,
    #[serde(default)]
    write: bool,
}

fn valid_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 63
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn plain(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= max && !text.chars().any(char::is_control)
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, CpError> {
    if body.len() > MAX_BODY {
        return Err(CpError::new(CpCode::BodyTooLarge));
    }
    serde_json::from_slice(body).map_err(|_| CpError::with_reason(CpCode::Invalid, "support.body"))
}

/// Who is acting, for the audit record.
struct Who<'a> {
    request_id: &'a str,
    principal: String,
    fingerprint: Option<&'a str>,
    command: Option<&'a str>,
}

/// The service: the store, the clock, the audit sink and the limits.
pub struct SupportService {
    store: Arc<dyn SupportStore>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditSink>,
    policy: Policy,
}

fn now_secs(clock: &dyn Clock) -> i64 {
    clock.now_utc().unix_timestamp()
}

impl SupportService {
    /// A service over `store`.
    #[must_use]
    pub fn new(
        store: Arc<dyn SupportStore>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditSink>,
        policy: Policy,
    ) -> Self {
        Self {
            store,
            clock,
            audit,
            policy,
        }
    }

    /// Writes one audit record; the effect that needs it is not carried out if this fails.
    async fn record(
        &self,
        who: &Who<'_>,
        action: &str,
        outcome: AuditOutcome,
        reason: Option<&str>,
    ) -> Result<(), CpError> {
        self.audit
            .record(AuditRecord {
                request_id: who.request_id.to_owned(),
                principal: Some(who.principal.clone()),
                certificate_fingerprint: who.fingerprint.map(str::to_owned),
                action: action.to_owned(),
                operation: None,
                risk: None,
                outcome,
                reason: reason.map(str::to_owned),
                command_id: who.command.map(str::to_owned),
                at: now_secs(self.clock.as_ref()),
            })
            .await
            .map(|_| ())
            .map_err(|_| CpError::with_reason(CpCode::Internal, "support.audit_failed"))
    }

    /// The customer's tenant host forwards an approved grant. The tenant is the certificate's.
    ///
    /// # Errors
    /// `cp.invalid` naming the fields, `cp.conflict` for a grant id already received,
    /// `cp.body_too_large`, `cp.internal`.
    pub async fn receive_grant(
        &self,
        host: &HostIdentity,
        request_id: &str,
        body: &Bytes,
    ) -> Result<Value, CpError> {
        let who = Who {
            request_id,
            principal: format!("host:{}/{}", host.tenant, host.host_id),
            fingerprint: Some(&host.fingerprint),
            command: None,
        };
        let outcome = self.receive_inner(host, body).await;
        let (result, reason) = match &outcome {
            Ok(_) => (AuditOutcome::Succeeded, None),
            Err(error) => (AuditOutcome::Refused, Some(error.reason)),
        };
        // A grant is stored only if its record can be written first: record, then act.
        self.record(&who, "support.grant_received", result, reason)
            .await?;
        let grant = outcome?;
        if !self.store.insert_grant(&grant).await? {
            let _ = self
                .record(
                    &who,
                    "support.grant_received",
                    AuditOutcome::Refused,
                    Some("support.grant_exists"),
                )
                .await;
            return Err(CpError::with_reason(
                CpCode::Conflict,
                "support.grant_exists",
            ));
        }
        let state = grant.state(now_secs(self.clock.as_ref()));
        Ok(json!({"id": grant.id, "state": state, "expires_at": grant.expires_at}))
    }

    async fn receive_inner(&self, host: &HostIdentity, body: &Bytes) -> Result<Grant, CpError> {
        let forwarded: ForwardedGrant = parse(body)?;
        let now = now_secs(self.clock.as_ref());
        let mut bad: Vec<&str> = Vec::new();
        if !valid_id(&forwarded.id) {
            bad.push("id");
        }
        if forwarded.doctypes.is_empty()
            || forwarded.doctypes.len() > MAX_DOCTYPES
            || forwarded.doctypes.iter().any(|d| !plain(d, 100))
        {
            bad.push("doctypes");
        }
        if !plain(&forwarded.approver, 200) {
            bad.push("approver");
        }
        if forwarded.record.is_empty() || forwarded.record.len() > MAX_RECORD {
            bad.push("record");
        }
        if forwarded.signature.is_empty() || forwarded.signature.len() > MAX_SIGNATURE {
            bad.push("signature");
        }
        let window = forwarded.expires_at.saturating_sub(forwarded.starts_at);
        if window <= 0 || window > self.policy.max_grant_secs {
            bad.push("expires_at");
        }
        if forwarded.expires_at <= now {
            bad.push("expires_at");
        }
        if forwarded.starts_at < now.saturating_sub(self.policy.backdate_secs) {
            bad.push("starts_at");
        }
        if !bad.is_empty() {
            bad.dedup();
            return Err(CpError::invalid(&bad));
        }
        let mut doctypes = forwarded.doctypes;
        doctypes.sort();
        doctypes.dedup();
        Ok(Grant {
            tenant: host.tenant.clone(),
            id: forwarded.id,
            doctypes,
            can_write: forwarded.write,
            starts_at: forwarded.starts_at,
            expires_at: forwarded.expires_at,
            approver: forwarded.approver,
            record: forwarded.record,
            received_at: now,
            revoked_at: None,
            revoked_by: None,
        })
    }

    /// The customer revoked a grant: immediate and final. The record is written first.
    ///
    /// # Errors
    /// `cp.not_found` for a grant of another tenant or an unknown one, `cp.invalid`,
    /// `cp.internal`.
    pub async fn revoke_grant(
        &self,
        host: &HostIdentity,
        request_id: &str,
        id: &str,
    ) -> Result<Value, CpError> {
        let who = Who {
            request_id,
            principal: format!("host:{}/{}", host.tenant, host.host_id),
            fingerprint: Some(&host.fingerprint),
            command: None,
        };
        if self.store.get_grant(&host.tenant, id).await?.is_none() {
            self.record(
                &who,
                "support.grant_revoked",
                AuditOutcome::Refused,
                Some("support.not_found"),
            )
            .await?;
            return Err(CpError::with_reason(CpCode::NotFound, "support.not_found"));
        }
        self.record(&who, "support.grant_revoked", AuditOutcome::Succeeded, None)
            .await?;
        let now = now_secs(self.clock.as_ref());
        let grant = self
            .store
            .revoke_grant(&host.tenant, id, "customer", now)
            .await?
            .ok_or_else(|| CpError::new(CpCode::NotFound))?;
        Ok(json!({"id": grant.id, "state": "revoked", "revoked_at": grant.revoked_at}))
    }

    /// The sessions opened against one of the host's own grants.
    ///
    /// # Errors
    /// `cp.not_found`, `cp.internal`.
    pub async fn host_sessions(&self, host: &HostIdentity, id: &str) -> Result<Value, CpError> {
        if self.store.get_grant(&host.tenant, id).await?.is_none() {
            return Err(CpError::new(CpCode::NotFound));
        }
        let sessions = self.store.sessions(&host.tenant, id, MAX_LIST).await?;
        Ok(json!(sessions.iter().map(session_json).collect::<Vec<_>>()))
    }

    /// The grants in the operator's scope, with their state.
    ///
    /// # Errors
    /// `cp.internal`.
    pub async fn list_grants(&self, call: &OperatorCall) -> Result<Value, CpError> {
        let now = now_secs(self.clock.as_ref());
        let grants = call
            .scope
            .retain(self.store.list_grants().await?, |g| g.tenant.as_str());
        Ok(json!(
            grants
                .iter()
                .map(|g| json!({
                    "tenant": g.tenant,
                    "id": g.id,
                    "scope": g.doctypes,
                    "write": g.can_write,
                    "starts_at": g.starts_at,
                    "expires_at": g.expires_at,
                    "state": g.state(now),
                }))
                .collect::<Vec<_>>()
        ))
    }

    /// Opens a session against an **active** grant. No grant, no session.
    ///
    /// # Errors
    /// `cp.not_found` (out of scope, unknown), `cp.forbidden` (expired, revoked, not started, a
    /// write the customer did not allow, too many open), `cp.invalid`, `cp.internal`.
    pub async fn open_session(
        &self,
        call: &OperatorCall,
        tenant: &str,
        id: &str,
        body: &Bytes,
    ) -> Result<Value, CpError> {
        call.scope.require(tenant)?;
        let command = call
            .command
            .as_ref()
            .ok_or_else(|| CpError::with_reason(CpCode::Forbidden, "support.command_required"))?;
        let parsed: OpenBody = parse(body)?;
        if !plain(&parsed.reason, 500) {
            return Err(CpError::invalid(&["reason"]));
        }
        let who = Who {
            request_id: &call.request_id,
            principal: call.principal.clone(),
            fingerprint: None,
            command: Some(&command.id),
        };
        let now = now_secs(self.clock.as_ref());
        let grant = self
            .store
            .get_grant(tenant, id)
            .await?
            .ok_or_else(|| CpError::with_reason(CpCode::NotFound, "support.no_grant"))?;
        let refuse = |reason: &'static str| CpError::with_reason(CpCode::Forbidden, reason);
        let state = grant.state(now);
        let denial = match state {
            "revoked" => Some("support.grant_revoked"),
            "expired" => Some("support.grant_expired"),
            "scheduled" => Some("support.grant_not_started"),
            _ if parsed.write && !grant.can_write => Some("support.write_not_granted"),
            _ => None,
        };
        if let Some(reason) = denial {
            self.record(
                &who,
                "support.session_open",
                AuditOutcome::Refused,
                Some(reason),
            )
            .await?;
            return Err(refuse(reason));
        }
        let session = Session {
            tenant: tenant.to_owned(),
            grant_id: id.to_owned(),
            id: format!(
                "ss-{}",
                &crate::auth::sha256_hex(
                    format!("{}|{}|{}|{}", tenant, id, command.id, now).as_bytes()
                )[..24]
            ),
            operator: call.principal.clone(),
            reason: parsed.reason,
            read_only: !(parsed.write && grant.can_write),
            opened_at: now,
            expires_at: grant
                .expires_at
                .min(now.saturating_add(self.policy.max_session_secs)),
            closed_at: None,
            command_id: Some(command.id.clone()),
        };
        // The record is written before the session exists; a session whose record cannot be
        // written is not opened.
        self.record(&who, "support.session_open", AuditOutcome::Succeeded, None)
            .await?;
        if !self
            .store
            .open_session(&session, self.policy.max_open_sessions, now)
            .await?
        {
            return Err(refuse("support.too_many_sessions"));
        }
        Ok(session_json(&session))
    }

    /// Closes a session of a grant in the operator's scope.
    ///
    /// # Errors
    /// `cp.not_found`, `cp.internal`.
    pub async fn close_session(
        &self,
        call: &OperatorCall,
        tenant: &str,
        id: &str,
        session: &str,
    ) -> Result<Value, CpError> {
        call.scope.require(tenant)?;
        let now = now_secs(self.clock.as_ref());
        let who = Who {
            request_id: &call.request_id,
            principal: call.principal.clone(),
            fingerprint: None,
            command: None,
        };
        let closed = self.store.close_session(tenant, id, session, now).await?;
        let outcome = if closed {
            AuditOutcome::Succeeded
        } else {
            AuditOutcome::Refused
        };
        self.record(
            &who,
            "support.session_close",
            outcome,
            (!closed).then_some("support.not_found"),
        )
        .await?;
        if closed {
            Ok(json!({"id": session, "closed_at": now}))
        } else {
            Err(CpError::new(CpCode::NotFound))
        }
    }

    /// The sessions of a grant in the operator's scope.
    ///
    /// # Errors
    /// `cp.not_found`, `cp.internal`.
    pub async fn list_sessions(
        &self,
        call: &OperatorCall,
        tenant: &str,
        id: &str,
    ) -> Result<Value, CpError> {
        call.scope.require(tenant)?;
        if self.store.get_grant(tenant, id).await?.is_none() {
            return Err(CpError::new(CpCode::NotFound));
        }
        let sessions = self.store.sessions(tenant, id, MAX_LIST).await?;
        Ok(json!(sessions.iter().map(session_json).collect::<Vec<_>>()))
    }
}

fn session_json(s: &Session) -> Value {
    json!({
        "id": s.id,
        "grant_id": s.grant_id,
        "operator": s.operator,
        "reason": s.reason,
        "read_only": s.read_only,
        "opened_at": s.opened_at,
        "expires_at": s.expires_at,
        "closed_at": s.closed_at,
    })
}

#[allow(clippy::result_large_err)]
fn answer(result: Result<Value, CpError>, request_id: &str) -> Result<Json<Value>, Response> {
    result
        .map(|data| Json(json!({ "data": data })))
        .map_err(|error| error.into_response_with(request_id))
}

type Svc = State<Arc<SupportService>>;

/// The operator routes of the module over `service`.
pub fn operator_router(service: Arc<SupportService>) -> Router {
    #[allow(clippy::result_large_err)]
    async fn list(
        State(s): Svc,
        Extension(call): Extension<OperatorCall>,
    ) -> Result<Json<Value>, Response> {
        answer(s.list_grants(&call).await, &call.request_id)
    }
    #[allow(clippy::result_large_err)]
    async fn open(
        State(s): Svc,
        Extension(call): Extension<OperatorCall>,
        Path((tenant, id)): Path<(String, String)>,
        body: Bytes,
    ) -> Result<Json<Value>, Response> {
        answer(
            s.open_session(&call, &tenant, &id, &body).await,
            &call.request_id,
        )
    }
    #[allow(clippy::result_large_err)]
    async fn sessions(
        State(s): Svc,
        Extension(call): Extension<OperatorCall>,
        Path((tenant, id)): Path<(String, String)>,
    ) -> Result<Json<Value>, Response> {
        answer(s.list_sessions(&call, &tenant, &id).await, &call.request_id)
    }
    #[allow(clippy::result_large_err)]
    async fn close(
        State(s): Svc,
        Extension(call): Extension<OperatorCall>,
        Path((tenant, id, session)): Path<(String, String, String)>,
    ) -> Result<Json<Value>, Response> {
        answer(
            s.close_session(&call, &tenant, &id, &session).await,
            &call.request_id,
        )
    }
    Router::new()
        .route("/cp/v1/support-grants", get(list))
        .route(
            "/cp/v1/support-grants/{tenant}/{id}/sessions",
            get(sessions).post(open),
        )
        .route(
            "/cp/v1/support-grants/{tenant}/{id}/sessions/{session}/close",
            post(close),
        )
        .with_state(service)
}

/// The host routes of the module over `service`. The tenant is the certificate's, always.
pub fn host_router(service: Arc<SupportService>) -> Router {
    #[allow(clippy::result_large_err)]
    async fn receive(
        State(s): Svc,
        Extension(host): Extension<HostIdentity>,
        Extension(request): Extension<RequestId>,
        body: Bytes,
    ) -> Result<Json<Value>, Response> {
        answer(s.receive_grant(&host, &request.0, &body).await, &request.0)
    }
    #[allow(clippy::result_large_err)]
    async fn revoke(
        State(s): Svc,
        Extension(host): Extension<HostIdentity>,
        Extension(request): Extension<RequestId>,
        Path(id): Path<String>,
    ) -> Result<Json<Value>, Response> {
        answer(s.revoke_grant(&host, &request.0, &id).await, &request.0)
    }
    #[allow(clippy::result_large_err)]
    async fn sessions(
        State(s): Svc,
        Extension(host): Extension<HostIdentity>,
        Extension(request): Extension<RequestId>,
        Path(id): Path<String>,
    ) -> Result<Json<Value>, Response> {
        answer(s.host_sessions(&host, &id).await, &request.0)
    }
    Router::new()
        .route("/cp/v1/host/support-grants", post(receive))
        .route("/cp/v1/host/support-grants/{id}/revoke", post(revoke))
        .route("/cp/v1/host/support-grants/{id}/sessions", get(sessions))
        .with_state(service)
}

/// Registers the module: its routes, its DDL and its readiness check. Needs the control database.
///
/// # Errors
/// Never today; the signature is the registry's.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    ctx.add_migration(DDL);
    // Grants are evidence and are never held in memory: without the control database the
    // module registers nothing (its DDL still applies when a pool exists).
    let Some(pool) = ctx.pool.clone() else {
        return Ok(());
    };
    let store: Arc<dyn SupportStore> = Arc::new(PgStore::new(pool));
    let service = Arc::new(SupportService::new(
        store,
        Arc::clone(&ctx.clock),
        Arc::clone(&ctx.audit),
        Policy::default(),
    ));
    ctx.operator_routes(operator_router(Arc::clone(&service)));
    ctx.host_routes(host_router(Arc::clone(&service)));
    let probe = Arc::clone(&service);
    ctx.add_readiness(Arc::new(move || {
        let probe = Arc::clone(&probe);
        Box::pin(async move { probe.store.list_grants().await.is_ok() })
    }));
    Ok(())
}
