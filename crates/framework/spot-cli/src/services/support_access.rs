//! The customer's side of deep support access (WP-SEC-07.T2, ADR-070 s9, ADR-44), over
//! `spot-deep-access` (the rules) and `spot-deep-access-pg` (the state).
//!
//! The vendor's access to customer data is **default deny**. A support request conveys no
//! authority: the platform operator's request arrives through [`SupportIntake::submit`] and is
//! stored `pending`; the tenant's security administrators see it (scope, tier, window, why, who),
//! and only a **customer-signed consent** from an enrolled key of the administrator's own identity,
//! verified here, moves it forward. Nothing is issued without that consent: [`SupportIntake::issue`]
//! refuses a request that is not `approved` and then runs the crate's `issue`, which re-verifies
//! everything and consumes the single-use receipt nonce atomically in PostgreSQL (shared by every
//! node, kept across restarts).
//!
//! # What the customer sees and does
//!
//! Through the `SupportAccessPort` of `spot-http` (this module implements it; the routes are
//! served by the route registry): the requests, the sessions (active, past, revoked) and the
//! actions taken under a session, read from the tenant audit chain (`SupportRecordings`), never
//! anything the operator could edit; consent (with a second administrator when the policy says
//! so); revocation, effective on the next `authorise` (nothing is cached) and recorded first.
//!
//! # Where things come from (typed ports, provided before this module's `ORDER`)
//!
//! - `dyn SupportCaller`: who the request's credential names, and whether they are a security
//!   administrator and carry a recent step-up. Any error resolving the caller is "no caller":
//!   a refusal, never an allow.
//! - `dyn SupportAudit`: appends one record to the tenant audit chain. Every outcome writes one,
//!   refusals included; an effect that needs its record is not carried out if the record cannot
//!   be written.
//! - `dyn SupportRecordings`: reads the actions of one session from the chain.
//! - For issuance: the shared `Bridge` (the nonce store is synchronous), `dyn EntropySource`
//!   (48 fresh CSPRNG bytes, never the operator's), `dyn SessionLogOpener` (the tenant's session
//!   log) and the vendor attestation verifier. Without them the intake still takes requests, the
//!   customer can still decide, and `issue` answers `Unavailable`.
//!
//! # Level
//!
//! Optional. The service's start-up check creates the tables and verifies the audit port exists;
//! a failure shows the site degraded and every operation answers `server.unavailable` (nothing is
//! decided without an audit record). `[services.support_access] enabled = false` registers nothing.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use spot_core::Clock;
use spot_deep_access::{
    DeepCredential, DeepError, OperatorAttestation, Policy, SessionLog, VendorAttestation,
};
use spot_deep_access_pg::PgDeepAccessStore;
use spot_http::api::ports::security_audit::{AuditCall, AuditReply, Op, SupportAccessPort};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_pg_bridge::Bridge;
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "support_access/engine.rs"]
mod engine;
#[path = "support_access/issuance.rs"]
mod issuance;

#[cfg(test)]
#[path = "support_access/tests.rs"]
mod tests;

pub use engine::{Refusal, SupportAccess, SupportStore};

/// Start order: after every module that provides a caller, audit, recordings or issuance port.
pub const ORDER: u32 = 2010;

/// `[services.support_access]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module is loaded. On by default; off registers nothing.
    pub enabled: bool,
    /// Approvals a request needs (1 or 2).
    pub approvals_required: u8,
    /// A request that may write (tier above T0) always needs two different administrators.
    pub two_person_for_write: bool,
    /// Seconds between sweeps of expired state (at least 60).
    pub sweep_every_secs: u64,
    /// Days a used receipt nonce is kept (at least 90; a receipt's window is hours).
    pub nonce_keep_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            approvals_required: 1,
            two_person_for_write: true,
            sweep_every_secs: 600,
            nonce_keep_days: 400,
        }
    }
}

impl Settings {
    fn validate(&self) -> Result<()> {
        if !(1..=2).contains(&self.approvals_required) {
            return Err(Error(
                "services.support_access.approvals_required must be 1 or 2".to_owned(),
            ));
        }
        if self.sweep_every_secs < 60 {
            return Err(Error(
                "services.support_access.sweep_every_secs must be at least 60".to_owned(),
            ));
        }
        if self.nonce_keep_days < 90 {
            return Err(Error(
                "services.support_access.nonce_keep_days must be at least 90".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Who a request's credential names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerInfo {
    /// The signed-in user's identity (the approver name a consent must carry).
    pub user_id: String,
    /// Whether the user is a security administrator of this tenant (decided by the permission
    /// engine; an engine error is "no caller").
    pub is_security_admin: bool,
    /// Whether the session carries a recent step-up.
    pub step_up_current: bool,
}

/// Resolves the caller of a request.
pub trait SupportCaller: Send + Sync {
    /// `None` when there is no credential, it does not resolve, or the permission engine could not
    /// answer: every `None` is a refusal.
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Option<CallerInfo>>;
}

/// One record for the tenant audit chain. No field carries a secret, a key, a signature or a
/// document value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupportAuditEvent {
    /// `support.consent`, `support.revoke`, ...
    pub kind: &'static str,
    /// The request or session id the event is about (`-` for a listing).
    pub subject: String,
    /// The acting identity (`operator` for the intake).
    pub actor: String,
    /// `approved`, `denied`, `not_found`, a `deep_access.*` code, ...
    pub outcome: &'static str,
    /// Unix seconds.
    pub at: i64,
}

/// The tenant audit chain, from the support service's point of view.
pub trait SupportAudit: Send + Sync {
    /// Appends the record durably. An error means it was not written.
    fn record<'a>(
        &'a self,
        event: SupportAuditEvent,
    ) -> BoxFuture<'a, std::result::Result<(), String>>;
}

/// One action taken under a session, as the chain recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedAction {
    /// RFC 3339 date-time.
    pub at: String,
    /// The action name (at most 200 characters).
    pub action: String,
    /// What it acted on (at most 500 characters), if anything.
    pub target: Option<String>,
}

/// Reads a session's actions from the tenant audit chain.
pub trait SupportRecordings: Send + Sync {
    /// At most `limit` actions of the session (32 lower-case hex characters), oldest first.
    fn actions<'a>(
        &'a self,
        session: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, std::result::Result<Vec<RecordedAction>, String>>;
}

/// 48 fresh CSPRNG bytes (16 for the session id, 32 for the secret), from the platform.
pub trait EntropySource: Send + Sync {
    /// Fills `out`.
    fn fill(&self, out: &mut [u8; 48]);
}

/// Opens the tenant's session log for one issuance (a chain writer; never passed to the operator).
pub trait SessionLogOpener: Send + Sync {
    /// A writer for the tenant's chain.
    fn open(&self) -> Box<dyn SessionLog + Send>;
}

/// What the operator side hands to [`SupportIntake::issue`]: no trusted input.
pub struct IssueRequest {
    /// The request to issue for (must be `approved`).
    pub request_id: String,
    /// The operator's hardware key.
    pub operator_key: [u8; 32],
    /// The vendor's statement about that key.
    pub attestation: OperatorAttestation,
}

/// Why an intake call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntakeError {
    /// The engine refused (the request is malformed, for another tenant, a duplicate, not
    /// approved).
    Refused(Refusal),
    /// `spot-deep-access` refused the issuance, with its stable code.
    Deep(DeepError),
}

/// The in-process port the management link calls on the tenant host.
pub trait SupportIntake: Send + Sync {
    /// Stores an operator's request as `pending`; returns its id. Conveys no authority.
    fn submit<'a>(
        &'a self,
        request_json: &'a str,
    ) -> BoxFuture<'a, std::result::Result<String, IntakeError>>;

    /// Issues a credential for an **approved** request. Everything is re-verified by
    /// `spot-deep-access`; a replayed receipt nonce, a wrong key or an expired consent is refused.
    fn issue<'a>(
        &'a self,
        request: IssueRequest,
    ) -> BoxFuture<'a, std::result::Result<DeepCredential, IntakeError>>;
}

/// Standard base64 (with padding) decoded to at most `max` bytes; `None` for anything else.
pub(crate) fn base64_decode(text: &str, max: usize) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) || bytes.len() / 4 * 3 > max + 2 {
        return None;
    }
    let value = |b: u8| match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let chunks = bytes.chunks(4);
    let last = chunks.len() - 1;
    for (i, chunk) in chunks.enumerate() {
        let pad = chunk.iter().rev().take_while(|b| **b == b'=').count();
        if pad > 2 || (pad > 0 && i != last) {
            return None;
        }
        let mut n = 0_u32;
        for (j, b) in chunk.iter().enumerate() {
            let v = if j >= 4 - pad {
                0
            } else {
                u32::from(value(*b)?)
            };
            n = (n << 6) | v;
        }
        let triple = n.to_be_bytes();
        out.extend_from_slice(&triple[1..4 - pad]);
    }
    (out.len() <= max).then_some(out)
}

fn api_error(refusal: &Refusal) -> ApiError {
    match refusal {
        Refusal::Denied => ApiError::from_code(Code::PermissionDenied),
        Refusal::StepUp => ApiError::from_code(Code::AuthStepUpRequired),
        Refusal::NotFound => ApiError::from_code(Code::SecurityNotFound),
        Refusal::NotPending => ApiError::from_code(Code::SecurityRequestNotPending),
        Refusal::ConsentInvalid(code) => {
            let mut error = ApiError::new(Code::SecurityConsentInvalid, "The consent was refused.");
            error.details.insert(
                "reason".to_owned(),
                serde_json::Value::String((*code).to_owned()),
            );
            error
        }
        Refusal::Invalid(message) => ApiError::new(Code::SecurityInvalid, *message),
        Refusal::Unavailable => ApiError::from_code(Code::ServerUnavailable),
    }
}

/// The cursor of a listing: bound to the caller and to the query digest, so a cursor replayed by
/// another user, on another operation or with other filters is refused like a malformed one.
fn cursor_make(user: &str, digest: &str, after: &str) -> String {
    format!("{}.{after}", cursor_tag(user, digest))
}

fn cursor_tag(user: &str, digest: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"spot.support_access.cursor\0");
    h.update(user.as_bytes());
    h.update(b"\0");
    h.update(digest.as_bytes());
    h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn cursor_parse<'a>(cursor: &'a str, user: &str, digest: &str) -> Option<&'a str> {
    let (tag, after) = cursor.split_once('.')?;
    (tag == cursor_tag(user, digest) && after.len() <= 200).then_some(after)
}

fn now_secs(clock: &dyn Clock) -> i64 {
    clock.now_utc().unix_timestamp()
}

/// The service: the engine, the ports, the clock and the database handle for the sweep.
pub struct SupportAccessService {
    engine: SupportAccess,
    caller: Arc<dyn SupportCaller>,
    clock: Arc<dyn Clock>,
    pg: PgDeepAccessStore,
    issuance: issuance::Issuance,
    audit_present: bool,
}

impl SupportAccessService {
    async fn dispatch(
        &self,
        ctx: &RequestCtx,
        call: &AuditCall,
    ) -> std::result::Result<AuditReply, ApiError> {
        let now = now_secs(self.clock.as_ref());
        let caller = self.caller.caller(ctx).await;
        let id = call.path.first().map_or("", String::as_str);
        let digest = call.page.as_ref().map_or("", |p| p.query_digest.as_str());
        let limit = call.page.as_ref().map_or(50, |p| p.limit);
        let user = caller.as_ref().map_or("", |c| c.user_id.as_str());
        let reply = |data: serde_json::Value, next: Option<Option<String>>| AuditReply {
            data: Some(data),
            next_cursor: next,
            export: None,
        };
        let result = match call.op {
            Op::SupportRequestList => {
                let after = match call.page.as_ref().and_then(|p| p.cursor.as_deref()) {
                    None => None,
                    Some(cursor) => Some(cursor_parse(cursor, user, digest).ok_or_else(|| {
                        ApiError::new(
                            Code::SecurityInvalid,
                            "The cursor is not valid for this request.",
                        )
                    })?),
                };
                // One more than the page, to know whether another page follows.
                match self
                    .engine
                    .list_requests(&caller, after, limit.saturating_add(1), now)
                    .await
                {
                    Ok(mut rows) => {
                        let more = rows.len() > limit as usize;
                        rows.truncate(limit as usize);
                        let next = if more {
                            rows.last()
                                .and_then(|r| r.get("id"))
                                .and_then(|v| v.as_str())
                                .map(|last| cursor_make(user, digest, last))
                        } else {
                            None
                        };
                        Ok(reply(serde_json::Value::Array(rows), Some(next)))
                    }
                    Err(refusal) => Err(refusal),
                }
            }
            Op::SupportRequestConsent => {
                let body = call.body.as_ref();
                let text = |k: &str| body.and_then(|b| b.get(k)).and_then(|v| v.as_str());
                match (text("consent"), text("signature")) {
                    (Some(record), Some(signature)) => self
                        .engine
                        .consent(&caller, id, record, signature, now)
                        .await
                        .map(|data| reply(data, None)),
                    _ => Err(Refusal::Invalid(
                        "a consent needs `consent` and `signature`",
                    )),
                }
            }
            Op::SupportRequestRevoke => self
                .engine
                .revoke(&caller, id, "revoked by the customer", now)
                .await
                .map(|data| reply(data, None)),
            Op::SupportSessionList => self
                .engine
                .list_sessions(&caller, limit, now)
                .await
                .map(|rows| reply(serde_json::Value::Array(rows), Some(None))),
            Op::SupportSessionRecording => self
                .engine
                .recording(&caller, id, limit, now)
                .await
                .map(|rows| reply(serde_json::Value::Array(rows), Some(None))),
            _ => Err(Refusal::Invalid("not a support access operation")),
        };
        result.map_err(|refusal| api_error(&refusal))
    }
}

impl SupportAccessPort for SupportAccessService {
    fn step_up_current<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<bool, ApiError>> {
        Box::pin(async move {
            Ok(self
                .caller
                .caller(ctx)
                .await
                .is_some_and(|c| c.is_security_admin && c.step_up_current))
        })
    }

    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a AuditCall,
    ) -> BoxFuture<'a, std::result::Result<AuditReply, ApiError>> {
        Box::pin(self.dispatch(ctx, call))
    }
}

impl SupportIntake for SupportAccessService {
    fn submit<'a>(
        &'a self,
        request_json: &'a str,
    ) -> BoxFuture<'a, std::result::Result<String, IntakeError>> {
        Box::pin(async move {
            let now = now_secs(self.clock.as_ref());
            self.engine
                .receive(request_json, now)
                .await
                .map_err(IntakeError::Refused)
        })
    }

    fn issue<'a>(
        &'a self,
        request: IssueRequest,
    ) -> BoxFuture<'a, std::result::Result<DeepCredential, IntakeError>> {
        Box::pin(async move {
            let now = now_secs(self.clock.as_ref());
            self.issuance.issue(&self.engine, request, now).await
        })
    }
}

impl Service for SupportAccessService {
    fn name(&self) -> &'static str {
        "support_access"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if !self.audit_present {
                return Err(
                    "no support audit port is hosted: nothing can be decided without an audit record"
                        .to_owned(),
                );
            }
            self.pg.prepare().await.map_err(|error| error.to_string())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut tick =
                tokio::time::interval(Duration::from_secs(self.engine.settings.sweep_every_secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    _ = tick.tick() => {
                        let now = now_secs(self.clock.as_ref());
                        let keep = i64::from(self.engine.settings.nonce_keep_days) * 86_400;
                        // A failed sweep is retried on the next tick; it never stops the site.
                        let _ = self.pg.sweep(now, keep, 1_000).await;
                    }
                }
            }
        })
    }
}

/// A stand-in for a missing audit port: every record fails, so no decision is carried out.
struct NoAudit;

impl SupportAudit for NoAudit {
    fn record<'a>(
        &'a self,
        _: SupportAuditEvent,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async { Err("no support audit port is hosted".to_owned()) })
    }
}

/// A stand-in for a missing caller port: nobody is a caller.
struct NoCaller;

impl SupportCaller for NoCaller {
    fn caller<'a>(&'a self, _: &'a RequestCtx) -> BoxFuture<'a, Option<CallerInfo>> {
        Box::pin(async { None })
    }
}

/// Registers the support-access service and its ports.
///
/// # Errors
/// A setting that does not decode or validate, or a port provided twice.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("support_access")?;
    settings.validate()?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let tenant = env.site.site.clone();
    let pg = PgDeepAccessStore::new(env.backend.pool(), &tenant)
        .map_err(|error| Error(error.to_string()))?;
    let audit = ctx.port::<dyn SupportAudit>();
    let audit_present = audit.is_some();
    let audit: Arc<dyn SupportAudit> = audit.unwrap_or_else(|| Arc::new(NoAudit));
    let engine = SupportAccess {
        store: Arc::new(pg.clone()),
        audit,
        recordings: ctx.port::<dyn SupportRecordings>(),
        settings,
        tenant,
    };
    let service = Arc::new(SupportAccessService {
        engine,
        caller: ctx
            .port::<dyn SupportCaller>()
            .unwrap_or_else(|| Arc::new(NoCaller)),
        clock: Arc::clone(ctx.clock()),
        pg,
        issuance: issuance::Issuance {
            bridge: ctx.port::<Bridge>(),
            entropy: ctx.port::<dyn EntropySource>(),
            logs: ctx.port::<dyn SessionLogOpener>(),
            verifier: ctx.port::<VendorAttestation>(),
            policy: Policy::default(),
        },
        audit_present,
    });
    ctx.provide::<dyn SupportAccessPort>(service.clone())?;
    ctx.provide::<dyn SupportIntake>(service.clone())?;
    ctx.add(service);
    Ok(())
}
