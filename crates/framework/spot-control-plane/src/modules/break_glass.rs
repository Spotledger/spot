//! The `break_glass` module (WP-ADM-11.T3): the control plane's side of break-glass, the vendor's
//! just-in-time raw-shell access to a customer's server.
//!
//! # Flow
//!
//! 1. **Request** (`POST /cp/v1/break-glass`): an operator names the tenant, a reason, the scope
//!    (what the session may touch) and a duration of at most four hours. The tenant must be inside
//!    the operator's scope; another tenant answers `cp.not_found`.
//! 2. **Approval** (`POST /cp/v1/break-glass/{id}/approve`): two *different* administrators each
//!    send a hardware-key signed approval that binds the tenant and the grant id (verified by a
//!    [`BreakGlassApprover`]). The requester cannot approve their own request, the same principal
//!    or the same key cannot approve twice, and nothing opens with one approval.
//! 3. **Open** (`POST .../open`): only an approved grant inside its window; the module asks the
//!    agent for `breakglass.open` with the *remaining* seconds. The agent enforces the end on its
//!    own clock; this module also marks the grant `expired` (the sweep) and asks the agent to
//!    close, but a lost message cannot extend a session.
//! 4. **Revoke** (`POST .../revoke`): final. An opened grant is closed on the agent
//!    (`breakglass.close`); if that cannot be sent it is retried by the sweep.
//! 5. **Close report** (`POST /cp/v1/host/break-glass/{id}/closed`, host route): the agent reports
//!    the sealed recording's digest; the tenant comes from the host's certificate, never the body.
//! 6. **Recording upload** (`PUT /cp/v1/host/break-glass/{id}/recording`): the recording is
//!    verified (hash chain and the reported digest) before it is kept.
//!
//! Every step, refusals included, is an audit record; none carries a reason text, a scope, a
//! signature or a terminal byte. State is a JSON row per grant in the module's key-value store.
//! Time is an argument (the injected clock).
//!
//! The agent's operations need a signed command path to the host (the management channel); until
//! a dispatcher is provided ([`AgentDispatch`]) opening fails closed with `cp.internal`.

pub use super::tenants_kv::{Kv, MemoryKv, PgKv};
use super::tenants_kv::{from_json, to_json};
use crate::auth::{HostIdentity, OperatorCall, sha256_hex};
use crate::{CpCode, CpCtx, CpError};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spot_break_glass::replay_bytes;
use spot_core::Clock;
use spot_mgmt_api::ports::{AuditOutcome, AuditRecord, AuditSink, BoxFuture};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// Registration order: after the tenants module.
pub const ORDER: u32 = 90;

const NS: &str = "break_glass";

/// The control-schema DDL: the shared key-value table (the same statement the tenants module runs).
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_tenants_kv (ns TEXT NOT NULL, k TEXT NOT NULL, v TEXT NOT NULL, PRIMARY KEY (ns, k))",
];
/// The shortest and longest grant, in seconds.
pub const MIN_SECONDS: u64 = 60;
/// The longest grant: four hours.
pub const MAX_SECONDS: u64 = 4 * 3600;
const MAX_RECORDING_BYTES: usize = 64 * 1024 * 1024;

/// A grant's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantState {
    /// Waiting for two approvals.
    Requested,
    /// Approved; the window runs from the approval.
    Approved,
    /// Opened on the agent.
    Opened,
    /// Closed by the agent, recording sealed.
    Closed,
    /// Revoked, final.
    Revoked,
    /// Past its window.
    Expired,
}

impl GrantState {
    fn is_over(self) -> bool {
        matches!(self, Self::Closed | Self::Revoked | Self::Expired)
    }
}

/// One approval: who (the operator principal) and with which key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// The operator principal that sent it.
    pub principal: String,
    /// The signing key's id.
    pub key: String,
    /// When (unix seconds).
    pub at: i64,
}

/// One break-glass grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The grant id.
    pub id: String,
    /// The tenant.
    pub tenant: String,
    /// The operator who asked.
    pub requester: String,
    /// Why (shown to the customer; never in an audit record).
    pub reason: String,
    /// What the session may touch.
    pub scope: Vec<String>,
    /// The window length.
    pub duration_seconds: u64,
    /// When it was asked.
    pub requested_at: i64,
    /// The approvals so far.
    pub approvals: Vec<Approval>,
    /// When the second approval landed.
    pub approved_at: Option<i64>,
    /// The end of the window.
    pub expires_at: Option<i64>,
    /// The state.
    pub state: GrantState,
    /// When it ended.
    pub ended_at: Option<i64>,
    /// A close the agent has not been told about yet.
    pub close_pending: bool,
    /// The sealed recording's digest (hex), once the agent reported it.
    pub recording_digest: Option<String>,
}

/// Verifies one signed approval of a grant.
pub trait BreakGlassApprover: Send + Sync {
    /// The signing key id when `approval` is a valid, fresh signed command that binds
    /// `"<tenant>:<grant id>"`; each approval is single use.
    fn verify<'a>(
        &'a self,
        approval: &'a str,
        tenant: &'a str,
        grant: &'a str,
        now: i64,
    ) -> BoxFuture<'a, Result<String, CpError>>;
}

/// The approver of a control plane that has no signing keys configured: nothing is approved.
pub struct RefusingApprover;

impl BreakGlassApprover for RefusingApprover {
    fn verify<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
        _: i64,
    ) -> BoxFuture<'a, Result<String, CpError>> {
        Box::pin(async { Err(CpError::new(CpCode::SignatureInvalid)) })
    }
}

/// Sends an operation to the host agent of a tenant (a signed command over the management
/// channel).
pub trait AgentDispatch: Send + Sync {
    /// Sends `op` (`breakglass.open` or `breakglass.close`) with `args`.
    fn send<'a>(
        &'a self,
        tenant: &'a str,
        op: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<(), CpError>>;
}

/// No dispatcher: every send fails, so nothing opens.
pub struct NoDispatch;

impl AgentDispatch for NoDispatch {
    fn send<'a>(&'a self, _: &'a str, _: &'a str, _: Value) -> BoxFuture<'a, Result<(), CpError>> {
        Box::pin(async {
            Err(CpError::with_reason(
                CpCode::Internal,
                "break_glass.dispatch_unavailable",
            ))
        })
    }
}

/// Where recordings are retained.
pub trait Recordings: Send + Sync {
    /// Keeps the verified recording of `grant`, once (a second put is refused).
    fn put<'a>(
        &'a self,
        tenant: &'a str,
        grant: &'a str,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), CpError>>;
}

/// In-memory retention, for tests and the development profile.
#[derive(Default)]
pub struct MemoryRecordings(Mutex<BTreeMap<(String, String), Vec<u8>>>);

impl MemoryRecordings {
    /// The retained bytes of `grant`, if any.
    pub fn get(&self, tenant: &str, grant: &str) -> Option<Vec<u8>> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&(tenant.to_owned(), grant.to_owned()))
            .cloned()
    }
}

impl Recordings for MemoryRecordings {
    fn put<'a>(
        &'a self,
        tenant: &'a str,
        grant: &'a str,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), CpError>> {
        Box::pin(async move {
            let mut rows = self.0.lock().unwrap_or_else(|p| p.into_inner());
            let key = (tenant.to_owned(), grant.to_owned());
            if rows.contains_key(&key) {
                return Err(CpError::new(CpCode::Conflict));
            }
            rows.insert(key, bytes);
            Ok(())
        })
    }
}

/// Retention not configured: uploads are refused.
pub struct NoRecordings;

impl Recordings for NoRecordings {
    fn put<'a>(&'a self, _: &'a str, _: &'a str, _: Vec<u8>) -> BoxFuture<'a, Result<(), CpError>> {
        Box::pin(async {
            Err(CpError::with_reason(
                CpCode::Internal,
                "break_glass.retention_unavailable",
            ))
        })
    }
}

/// What the module is built from.
pub struct Plane {
    kv: Arc<dyn Kv>,
    approver: Arc<dyn BreakGlassApprover>,
    dispatch: Arc<dyn AgentDispatch>,
    recordings: Arc<dyn Recordings>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditSink>,
    seq: Mutex<u64>,
}

fn valid_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

fn valid_grant_id(id: &str) -> bool {
    id.len() == 24
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl Plane {
    /// A plane over its ports.
    pub fn new(
        kv: Arc<dyn Kv>,
        approver: Arc<dyn BreakGlassApprover>,
        dispatch: Arc<dyn AgentDispatch>,
        recordings: Arc<dyn Recordings>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditSink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            kv,
            approver,
            dispatch,
            recordings,
            clock,
            audit,
            seq: Mutex::new(0),
        })
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    async fn audit(
        &self,
        call_principal: Option<&str>,
        request_id: &str,
        action: &str,
        outcome: AuditOutcome,
        reason: Option<&str>,
    ) {
        let _ = self
            .audit
            .record(AuditRecord {
                request_id: request_id.to_owned(),
                principal: call_principal.map(str::to_owned),
                certificate_fingerprint: None,
                action: action.to_owned(),
                operation: None,
                risk: None,
                outcome,
                reason: reason.map(str::to_owned),
                command_id: None,
                at: self.now(),
            })
            .await;
    }

    async fn load(&self, id: &str) -> Result<Grant, CpError> {
        if !valid_grant_id(id) {
            return Err(CpError::new(CpCode::NotFound));
        }
        match self.kv.get(NS, id).await? {
            Some(text) => from_json(&text),
            None => Err(CpError::new(CpCode::NotFound)),
        }
    }

    async fn load_in_scope(&self, call: &OperatorCall, id: &str) -> Result<Grant, CpError> {
        let grant = self.load(id).await?;
        // Out of scope is exactly a missing grant.
        call.scope.require(&grant.tenant)?;
        Ok(grant)
    }

    async fn save(&self, grant: &Grant) -> Result<(), CpError> {
        self.kv.put(NS, &grant.id, &to_json(grant)?).await
    }

    fn next_id(&self, requester: &str, tenant: &str) -> String {
        let n = {
            let mut seq = self.seq.lock().unwrap_or_else(|p| p.into_inner());
            *seq += 1;
            *seq
        };
        let seed = format!(
            "{requester}|{tenant}|{}|{n}",
            self.clock.now_utc().unix_timestamp_nanos()
        );
        sha256_hex(seed.as_bytes()).chars().take(24).collect()
    }

    /// Asks for a grant.
    ///
    /// # Errors
    ///
    /// `cp.not_found` for a tenant outside the operator's scope, `cp.invalid` for a bad field.
    pub async fn request(&self, call: &OperatorCall, req: RequestBody) -> Result<Grant, CpError> {
        call.scope.require(&req.tenant)?;
        let reason_ok = (10..=500).contains(&req.reason.chars().count())
            && !req.reason.chars().any(char::is_control);
        let scope_ok = !req.scope.is_empty()
            && req.scope.len() <= 16
            && req.scope.iter().all(|s| valid_name(s));
        let mut bad = Vec::new();
        if !valid_name(&req.tenant) {
            bad.push("tenant");
        }
        if !reason_ok {
            bad.push("reason");
        }
        if !scope_ok {
            bad.push("scope");
        }
        if !(MIN_SECONDS..=MAX_SECONDS).contains(&req.duration_seconds) {
            bad.push("duration_seconds");
        }
        if !bad.is_empty() {
            self.audit(
                Some(&call.principal),
                &call.request_id,
                "break_glass.request",
                AuditOutcome::Refused,
                Some("invalid"),
            )
            .await;
            return Err(CpError::invalid(&bad));
        }
        let grant = Grant {
            id: self.next_id(&call.principal, &req.tenant),
            tenant: req.tenant,
            requester: call.principal.clone(),
            reason: req.reason,
            scope: req.scope,
            duration_seconds: req.duration_seconds,
            requested_at: self.now(),
            approvals: Vec::new(),
            approved_at: None,
            expires_at: None,
            state: GrantState::Requested,
            ended_at: None,
            close_pending: false,
            recording_digest: None,
        };
        self.save(&grant).await?;
        self.audit(
            Some(&call.principal),
            &call.request_id,
            "break_glass.request",
            AuditOutcome::Allowed,
            None,
        )
        .await;
        Ok(grant)
    }

    /// Records one signed approval; the second distinct one approves the grant.
    ///
    /// # Errors
    ///
    /// `cp.forbidden` for the requester or a repeat approver, `cp.signature_invalid` for a bad
    /// approval, `cp.conflict` when the grant is not waiting for approvals.
    pub async fn approve(
        &self,
        call: &OperatorCall,
        id: &str,
        approval: &str,
    ) -> Result<Grant, CpError> {
        let mut grant = self.load_in_scope(call, id).await?;
        let refuse = |code: CpCode, reason: &'static str| CpError::with_reason(code, reason);
        let result = async {
            if grant.state != GrantState::Requested {
                return Err(refuse(CpCode::Conflict, "break_glass.not_waiting"));
            }
            if grant.requester == call.principal {
                return Err(refuse(CpCode::Forbidden, "break_glass.self_approval"));
            }
            if grant
                .approvals
                .iter()
                .any(|a| a.principal == call.principal)
            {
                return Err(refuse(CpCode::Forbidden, "break_glass.repeat_approver"));
            }
            let now = self.now();
            let key = self
                .approver
                .verify(approval, &grant.tenant, &grant.id, now)
                .await?;
            if grant.approvals.iter().any(|a| a.key == key) {
                return Err(refuse(CpCode::Forbidden, "break_glass.repeat_key"));
            }
            grant.approvals.push(Approval {
                principal: call.principal.clone(),
                key,
                at: now,
            });
            if grant.approvals.len() >= 2 {
                grant.state = GrantState::Approved;
                grant.approved_at = Some(now);
                grant.expires_at = Some(now + i64::try_from(grant.duration_seconds).unwrap_or(0));
            }
            self.save(&grant).await?;
            Ok(grant.clone())
        }
        .await;
        match &result {
            Ok(_) => {
                self.audit(
                    Some(&call.principal),
                    &call.request_id,
                    "break_glass.approve",
                    AuditOutcome::Allowed,
                    None,
                )
                .await
            }
            Err(e) => {
                self.audit(
                    Some(&call.principal),
                    &call.request_id,
                    "break_glass.approve",
                    AuditOutcome::Refused,
                    Some(e.code.as_str()),
                )
                .await
            }
        }
        result
    }

    /// Opens an approved grant on the agent with the seconds that remain of its window.
    ///
    /// # Errors
    ///
    /// `cp.conflict` when the grant is not approved or its window is over (an expired grant is
    /// marked so and can never be reopened), `cp.internal` when the agent cannot be reached.
    pub async fn open(&self, call: &OperatorCall, id: &str) -> Result<Grant, CpError> {
        let mut grant = self.load_in_scope(call, id).await?;
        let now = self.now();
        let result = async {
            if grant.state != GrantState::Approved {
                return Err(CpError::with_reason(
                    CpCode::Conflict,
                    "break_glass.not_approved",
                ));
            }
            let expires = grant.expires_at.unwrap_or(0);
            if now >= expires {
                grant.state = GrantState::Expired;
                grant.ended_at = Some(now);
                self.save(&grant).await?;
                return Err(CpError::with_reason(
                    CpCode::Conflict,
                    "break_glass.window_over",
                ));
            }
            let args = json!({
                "session": grant.id, "tenant": grant.tenant, "scope": grant.scope,
                "duration_seconds": expires - now,
            });
            self.dispatch
                .send(&grant.tenant, "breakglass.open", args)
                .await?;
            grant.state = GrantState::Opened;
            self.save(&grant).await?;
            Ok(grant.clone())
        }
        .await;
        let (outcome, reason) = match &result {
            Ok(_) => (AuditOutcome::Allowed, None),
            Err(e) => (AuditOutcome::Refused, Some(e.code.as_str())),
        };
        self.audit(
            Some(&call.principal),
            &call.request_id,
            "break_glass.open",
            outcome,
            reason,
        )
        .await;
        result
    }

    /// Revokes a grant: final. An opened grant is closed on the agent; a close that cannot be
    /// sent is retried by the sweep.
    ///
    /// # Errors
    ///
    /// `cp.conflict` when the grant is already over.
    pub async fn revoke(&self, call: &OperatorCall, id: &str) -> Result<Grant, CpError> {
        let mut grant = self.load_in_scope(call, id).await?;
        if grant.state.is_over() {
            self.audit(
                Some(&call.principal),
                &call.request_id,
                "break_glass.revoke",
                AuditOutcome::Refused,
                Some("over"),
            )
            .await;
            return Err(CpError::with_reason(
                CpCode::Conflict,
                "break_glass.already_over",
            ));
        }
        let was_opened = grant.state == GrantState::Opened;
        grant.state = GrantState::Revoked;
        grant.ended_at = Some(self.now());
        grant.close_pending = was_opened;
        self.save(&grant).await?;
        if was_opened {
            self.try_close(&mut grant).await;
        }
        self.audit(
            Some(&call.principal),
            &call.request_id,
            "break_glass.revoke",
            AuditOutcome::Allowed,
            None,
        )
        .await;
        Ok(grant)
    }

    async fn try_close(&self, grant: &mut Grant) {
        let sent = self
            .dispatch
            .send(
                &grant.tenant,
                "breakglass.close",
                json!({ "session": grant.id }),
            )
            .await
            .is_ok();
        if sent && grant.close_pending {
            grant.close_pending = false;
            let _ = self.save(grant).await;
        }
    }

    /// The grants of the tenants in the operator's scope, newest first.
    ///
    /// # Errors
    ///
    /// `cp.internal` when the store fails.
    pub async fn list(&self, call: &OperatorCall) -> Result<Vec<Grant>, CpError> {
        let mut grants: Vec<Grant> = Vec::new();
        for (_, text) in self.kv.list(NS).await? {
            grants.push(from_json(&text)?);
        }
        let mut grants = call.scope.retain(grants, |g| g.tenant.as_str());
        grants.sort_by(|a, b| b.requested_at.cmp(&a.requested_at).then(a.id.cmp(&b.id)));
        Ok(grants)
    }

    /// One sweep: ends every approved or opened grant past its window (the agent already did on
    /// its own clock) and retries every close that was not delivered. Returns how many ended.
    pub async fn sweep(&self) -> usize {
        let now = self.now();
        let Ok(rows) = self.kv.list(NS).await else {
            return 0;
        };
        let mut ended = 0;
        for (_, text) in rows {
            let Ok(mut grant) = from_json::<Grant>(&text) else {
                continue;
            };
            let due = matches!(grant.state, GrantState::Approved | GrantState::Opened)
                && grant.expires_at.is_some_and(|e| now >= e);
            if due {
                let was_opened = grant.state == GrantState::Opened;
                grant.state = GrantState::Expired;
                grant.ended_at = Some(now);
                grant.close_pending = was_opened;
                if self.save(&grant).await.is_ok() {
                    ended += 1;
                    self.audit(
                        None,
                        "sweep",
                        "break_glass.expire",
                        AuditOutcome::Allowed,
                        None,
                    )
                    .await;
                }
            }
            if grant.close_pending && grant.state.is_over() {
                self.try_close(&mut grant).await;
            }
        }
        ended
    }

    /// The agent's report of a sealed recording; the tenant is the host's certificate's.
    ///
    /// # Errors
    ///
    /// `cp.not_found` when the grant is not this host's tenant's, `cp.invalid` for a bad digest.
    pub async fn closed(
        &self,
        host: &HostIdentity,
        id: &str,
        digest: &str,
    ) -> Result<Grant, CpError> {
        let mut grant = self.load(id).await?;
        if grant.tenant != host.tenant {
            return Err(CpError::new(CpCode::NotFound));
        }
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(CpError::invalid(&["digest"]));
        }
        if grant
            .recording_digest
            .as_deref()
            .is_some_and(|d| d != digest)
        {
            return Err(CpError::with_reason(
                CpCode::Conflict,
                "break_glass.digest_changed",
            ));
        }
        grant.recording_digest = Some(digest.to_owned());
        if grant.state == GrantState::Opened {
            grant.state = GrantState::Closed;
            grant.ended_at = Some(self.now());
        }
        grant.close_pending = false;
        self.save(&grant).await?;
        self.audit(
            None,
            &host.host_id,
            "break_glass.closed",
            AuditOutcome::Allowed,
            None,
        )
        .await;
        Ok(grant)
    }

    /// Keeps an uploaded recording after verifying its hash chain and its digest.
    ///
    /// # Errors
    ///
    /// `cp.invalid` when the chain does not verify or the digest differs from the reported one,
    /// `cp.not_found` for another tenant's grant, `cp.body_too_large`.
    pub async fn upload(
        &self,
        host: &HostIdentity,
        id: &str,
        bytes: Vec<u8>,
    ) -> Result<(), CpError> {
        if bytes.len() > MAX_RECORDING_BYTES {
            return Err(CpError::new(CpCode::BodyTooLarge));
        }
        let grant = self.load(id).await?;
        if grant.tenant != host.tenant {
            return Err(CpError::new(CpCode::NotFound));
        }
        let Some(reported) = grant.recording_digest.clone() else {
            return Err(CpError::with_reason(
                CpCode::Conflict,
                "break_glass.not_closed",
            ));
        };
        let replay = replay_bytes(&bytes);
        if !replay.verified() || spot_break_glass::hex(&replay.digest()) != reported {
            self.audit(
                None,
                &host.host_id,
                "break_glass.upload",
                AuditOutcome::Refused,
                Some("unverified"),
            )
            .await;
            return Err(CpError::invalid(&["recording"]));
        }
        self.recordings.put(&grant.tenant, &grant.id, bytes).await?;
        self.audit(
            None,
            &host.host_id,
            "break_glass.upload",
            AuditOutcome::Allowed,
            None,
        )
        .await;
        Ok(())
    }
}

/// The body of a request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestBody {
    /// The tenant.
    pub tenant: String,
    /// Why.
    pub reason: String,
    /// What the session may touch.
    pub scope: Vec<String>,
    /// The window length, 60 to 14,400.
    pub duration_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApproveBody {
    approval: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosedBody {
    digest: String,
}

/// The operator's view of a grant: no approval signature is ever stored or shown.
fn view(grant: &Grant) -> Value {
    json!({
        "id": grant.id, "tenant": grant.tenant, "requester": grant.requester, "reason": grant.reason,
        "scope": grant.scope, "duration_seconds": grant.duration_seconds, "state": grant.state,
        "approvals": grant.approvals.iter().map(|a| json!({"principal": a.principal, "at": a.at})).collect::<Vec<_>>(),
        "requested_at": grant.requested_at, "approved_at": grant.approved_at,
        "expires_at": grant.expires_at, "ended_at": grant.ended_at,
        "recording_digest": grant.recording_digest,
    })
}

fn done(call: &OperatorCall, status: StatusCode, result: Result<Grant, CpError>) -> Response {
    match result {
        Ok(grant) => (status, Json(view(&grant))).into_response(),
        Err(error) => error.into_response_with(&call.request_id),
    }
}

async fn request_h(
    State(p): State<Arc<Plane>>,
    Extension(call): Extension<OperatorCall>,
    body: Bytes,
) -> Response {
    let parsed: Result<RequestBody, CpError> =
        serde_json::from_slice(&body).map_err(|_| CpError::invalid(&["body"]));
    let result = match parsed {
        Ok(req) => p.request(&call, req).await,
        Err(e) => Err(e),
    };
    done(&call, StatusCode::CREATED, result)
}

async fn list_h(State(p): State<Arc<Plane>>, Extension(call): Extension<OperatorCall>) -> Response {
    match p.list(&call).await {
        Ok(grants) => (
            StatusCode::OK,
            Json(json!({ "data": grants.iter().map(view).collect::<Vec<_>>() })),
        )
            .into_response(),
        Err(error) => error.into_response_with(&call.request_id),
    }
}

async fn approve_h(
    State(p): State<Arc<Plane>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let result = match serde_json::from_slice::<ApproveBody>(&body) {
        Ok(b) => p.approve(&call, &id, &b.approval).await,
        Err(_) => Err(CpError::invalid(&["body"])),
    };
    done(&call, StatusCode::OK, result)
}

async fn open_h(
    State(p): State<Arc<Plane>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = p.open(&call, &id).await;
    done(&call, StatusCode::OK, result)
}

async fn revoke_h(
    State(p): State<Arc<Plane>>,
    Extension(call): Extension<OperatorCall>,
    Path(id): Path<String>,
) -> Response {
    let result = p.revoke(&call, &id).await;
    done(&call, StatusCode::OK, result)
}

async fn closed_h(
    State(p): State<Arc<Plane>>,
    Extension(host): Extension<HostIdentity>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let result = match serde_json::from_slice::<ClosedBody>(&body) {
        Ok(b) => p.closed(&host, &id, &b.digest).await,
        Err(_) => Err(CpError::invalid(&["body"])),
    };
    match result {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response_with(&host.host_id),
    }
}

async fn upload_h(
    State(p): State<Arc<Plane>>,
    Extension(host): Extension<HostIdentity>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    match p.upload(&host, &id, body.to_vec()).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response_with(&host.host_id),
    }
}

/// The operator routes.
pub fn operator_routes(plane: Arc<Plane>) -> Router {
    Router::new()
        .route("/cp/v1/break-glass", post(request_h).get(list_h))
        .route("/cp/v1/break-glass/{id}/approve", post(approve_h))
        .route("/cp/v1/break-glass/{id}/open", post(open_h))
        .route("/cp/v1/break-glass/{id}/revoke", post(revoke_h))
        .with_state(plane)
}

/// The host routes (the tenant is the certificate's).
pub fn host_routes(plane: Arc<Plane>) -> Router {
    Router::new()
        .route("/cp/v1/host/break-glass/{id}/closed", post(closed_h))
        .route("/cp/v1/host/break-glass/{id}/recording", put(upload_h))
        .with_state(plane)
}

/// Mounts the module over a prepared plane: the routes, the expiry sweep and readiness.
pub fn register_with(ctx: &mut CpCtx, plane: Arc<Plane>) {
    ctx.operator_routes(operator_routes(Arc::clone(&plane)));
    ctx.host_routes(host_routes(Arc::clone(&plane)));
    let sweeper = Arc::clone(&plane);
    ctx.add_task(Box::new(
        move |mut stop: watch::Receiver<bool>| -> BoxFuture<'static, ()> {
            Box::pin(async move {
                loop {
                    if *stop.borrow() {
                        return;
                    }
                    tokio::select! {
                        changed = stop.changed() => {
                            if changed.is_err() {
                                return;
                            }
                        }
                        () = tokio::time::sleep(Duration::from_secs(15)) => {
                            sweeper.sweep().await;
                        }
                    }
                }
            })
        },
    ));
}

/// Registers the module. Without the control database it mounts nothing and reports not ready;
/// with it, approvals are refused until a signed-approval verifier is configured and nothing can
/// be sent to an agent until a dispatcher is (both fail closed).
///
/// # Errors
///
/// Never today; the signature matches the registry's.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    let Some(pool) = ctx.pool.clone() else {
        ctx.add_readiness(Arc::new(|| -> BoxFuture<'static, bool> {
            Box::pin(async { false })
        }));
        return Ok(());
    };
    ctx.add_migration(DDL);
    let plane = Plane::new(
        Arc::new(PgKv::new(pool)),
        Arc::new(RefusingApprover),
        Arc::new(NoDispatch),
        Arc::new(NoRecordings),
        Arc::clone(&ctx.clock),
        Arc::clone(&ctx.audit),
    );
    register_with(ctx, plane);
    Ok(())
}
