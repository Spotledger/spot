//! The decisions of the support-access service, independent of HTTP and of the database
//! (WP-SEC-07.T2). The engine is generic over a small store trait and over typed ports, so its
//! rules are tested without PostgreSQL; the PostgreSQL store implements the trait.
//!
//! Every decision here is made on the server and **fails closed**: a missing caller, an error from
//! a port or a store, an unparsable record is a refusal, never an allow. Every outcome, refusals
//! included, writes an audit record. A cross-tenant id answers `NotFound` exactly like a missing
//! one (the store is scoped to the site's tenant, so another tenant's row cannot be read at all).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_deep_access::{
    AccessRequest, ConsentRecord, CustomerTrust, TrustedApprover, check_consent, to_hex,
};
use spot_deep_access_pg::{
    ConsentRow, CredentialRow, KeyRow, PgDeepAccessStore, RequestRow, RequestStatus, StoreError,
};

use super::{CallerInfo, Settings, SupportAudit, SupportAuditEvent, SupportRecordings};

/// The boxed future of a store call.
pub type SFut<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

/// What the engine needs of the database. `PgDeepAccessStore` implements it.
pub trait SupportStore: Send + Sync {
    /// See [`PgDeepAccessStore::put_request`].
    fn put_request<'a>(&'a self, row: &'a RequestRow, now: i64) -> SFut<'a, bool>;
    /// See [`PgDeepAccessStore::get_request`].
    fn get_request<'a>(&'a self, id: &'a str) -> SFut<'a, Option<RequestRow>>;
    /// See [`PgDeepAccessStore::list_requests`].
    fn list_requests<'a>(
        &'a self,
        status: Option<RequestStatus>,
        after: Option<&'a str>,
        limit: u32,
    ) -> SFut<'a, Vec<RequestRow>>;
    /// See [`PgDeepAccessStore::transition`].
    fn transition<'a>(
        &'a self,
        id: &'a str,
        from: &'a [RequestStatus],
        to: RequestStatus,
        now: i64,
    ) -> SFut<'a, bool>;
    /// See [`PgDeepAccessStore::record_consent`].
    fn record_consent<'a>(&'a self, row: &'a ConsentRow) -> SFut<'a, bool>;
    /// See [`PgDeepAccessStore::consents`].
    fn consents<'a>(&'a self, request_id: &'a str) -> SFut<'a, Vec<ConsentRow>>;
    /// See [`PgDeepAccessStore::revoke`].
    fn revoke<'a>(
        &'a self,
        session: [u8; 16],
        by: &'a str,
        reason: &'a str,
        at: i64,
    ) -> SFut<'a, ()>;
    /// See [`PgDeepAccessStore::credentials`].
    fn credentials<'a>(&'a self, limit: u32) -> SFut<'a, Vec<(CredentialRow, bool)>>;
    /// See [`PgDeepAccessStore::keys`].
    fn keys<'a>(&'a self) -> SFut<'a, Vec<KeyRow>>;
    /// See [`PgDeepAccessStore::enrol_key`].
    fn enrol_key<'a>(&'a self, row: &'a KeyRow) -> SFut<'a, bool>;
    /// See [`PgDeepAccessStore::record_credential`].
    fn record_credential<'a>(
        &'a self,
        row: &'a CredentialRow,
        secret_digest: [u8; 32],
    ) -> SFut<'a, ()>;
}

macro_rules! delegate {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {$(
        fn $name<'a>(&'a self, $($arg: $ty),*) -> SFut<'a, $ret> {
            Box::pin(PgDeepAccessStore::$name(self, $($arg),*))
        }
    )*};
}

impl SupportStore for PgDeepAccessStore {
    delegate! {
        put_request(row: &'a RequestRow, now: i64) -> bool;
        get_request(id: &'a str) -> Option<RequestRow>;
        list_requests(status: Option<RequestStatus>, after: Option<&'a str>, limit: u32) -> Vec<RequestRow>;
        transition(id: &'a str, from: &'a [RequestStatus], to: RequestStatus, now: i64) -> bool;
        record_consent(row: &'a ConsentRow) -> bool;
        consents(request_id: &'a str) -> Vec<ConsentRow>;
        revoke(session: [u8; 16], by: &'a str, reason: &'a str, at: i64) -> ();
        credentials(limit: u32) -> Vec<(CredentialRow, bool)>;
        keys() -> Vec<KeyRow>;
        enrol_key(row: &'a KeyRow) -> bool;
        record_credential(row: &'a CredentialRow, secret_digest: [u8; 32]) -> ();
    }
}

/// Why the engine refused. The HTTP layer maps each to a registry code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The caller may not do this (not a security administrator, or no caller at all).
    Denied,
    /// A step-up is required first.
    StepUp,
    /// No such request or session in this tenant.
    NotFound,
    /// The request is not pending (decided, revoked or expired).
    NotPending,
    /// The consent is refused; the `deep_access.*` code says why (never a value).
    ConsentInvalid(&'static str),
    /// The input is malformed or too large.
    Invalid(&'static str),
    /// A store, a port or the audit chain could not answer: nothing was done.
    Unavailable,
}

pub(super) fn from_store(_: StoreError) -> Refusal {
    Refusal::Unavailable
}

/// The decisions, for one site.
pub struct SupportAccess {
    pub(super) store: Arc<dyn SupportStore>,
    pub(super) audit: Arc<dyn SupportAudit>,
    pub(super) recordings: Option<Arc<dyn SupportRecordings>>,
    pub(super) settings: Settings,
    pub(super) tenant: String,
}

/// The customer-readable summary of a request.
fn request_json(row: &RequestRow, approvals: usize, needed: usize) -> serde_json::Value {
    let parsed: Option<AccessRequest> = serde_json::from_str(&row.request).ok();
    let (requester, reason, ticket, scope, tier, starts_at, window_end) = match &parsed {
        Some(r) => (
            r.requester.id.clone(),
            r.reason.clone(),
            r.ticket_ref.clone(),
            format!("{:?}", r.spec.scope),
            format!("{:?}", r.spec.tier_ceiling),
            r.spec.starts_at,
            r.spec.expires_at,
        ),
        None => (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            0,
            0,
        ),
    };
    serde_json::json!({
        "id": row.id,
        "status": row.status.as_str(),
        "requester": requester,
        "reason": reason,
        "ticket": ticket,
        "scope": scope,
        "tier": tier,
        "starts_at": starts_at,
        "expires_at": window_end,
        "requested_at": row.requested_at,
        "approvals": approvals,
        "approvals_needed": needed,
    })
}

impl SupportAccess {
    pub(super) fn needed_for(&self, request: &AccessRequest) -> usize {
        self.needed(request)
    }

    fn needed(&self, request: &AccessRequest) -> usize {
        let write = request.spec.tier_ceiling != spot_support_session::Tier::T0;
        if self.settings.two_person_for_write && write {
            2.max(usize::from(self.settings.approvals_required))
        } else {
            usize::from(self.settings.approvals_required)
        }
    }

    async fn audit(
        &self,
        kind: &'static str,
        subject: &str,
        actor: &str,
        outcome: &'static str,
        now: i64,
    ) {
        // An audit failure is not swallowed by the callers: they check `audit_must`.
        let _ = self
            .audit
            .record(SupportAuditEvent {
                kind,
                subject: subject.to_owned(),
                actor: actor.to_owned(),
                outcome,
                at: now,
            })
            .await;
    }

    /// Writes the audit record and refuses when it cannot be written (used before an effect).
    async fn audit_must(
        &self,
        kind: &'static str,
        subject: &str,
        actor: &str,
        outcome: &'static str,
        now: i64,
    ) -> Result<(), Refusal> {
        self.audit
            .record(SupportAuditEvent {
                kind,
                subject: subject.to_owned(),
                actor: actor.to_owned(),
                outcome,
                at: now,
            })
            .await
            .map_err(|_| Refusal::Unavailable)
    }

    /// Only a signed-in security administrator, with a step-up where the operation needs one.
    async fn gate(
        &self,
        caller: &Option<CallerInfo>,
        kind: &'static str,
        subject: &str,
        needs_step_up: bool,
        now: i64,
    ) -> Result<CallerInfo, Refusal> {
        let Some(caller) = caller.clone() else {
            self.audit(kind, subject, "-", "denied", now).await;
            return Err(Refusal::Denied);
        };
        if !caller.is_security_admin {
            self.audit(kind, subject, &caller.user_id, "denied", now)
                .await;
            return Err(Refusal::Denied);
        }
        if needs_step_up && !caller.step_up_current {
            self.audit(kind, subject, &caller.user_id, "step_up_required", now)
                .await;
            return Err(Refusal::StepUp);
        }
        Ok(caller)
    }

    /// The operator's request arrives (in-process port of the management link). It conveys no
    /// authority: it is stored `pending` for the customer to see, and an id already taken is
    /// refused, so a request cannot be replaced.
    ///
    /// # Errors
    /// [`Refusal::Invalid`] for a request that is not for this tenant, is malformed or too large;
    /// [`Refusal::Unavailable`].
    pub async fn receive(&self, request_json_text: &str, now: i64) -> Result<String, Refusal> {
        if request_json_text.len() > 65_536 {
            return Err(Refusal::Invalid("the request is over 64 KiB"));
        }
        let request: AccessRequest = serde_json::from_str(request_json_text)
            .map_err(|_| Refusal::Invalid("the request is not a support access request"))?;
        if request.spec.tenant != self.tenant {
            // Another tenant's request is not stored and not acknowledged as anything.
            self.audit(
                "support.request_received",
                &request.id,
                "operator",
                "wrong_tenant",
                now,
            )
            .await;
            return Err(Refusal::Invalid("the request is not for this tenant"));
        }
        let hash = request
            .hash()
            .map_err(|_| Refusal::Invalid("the request cannot be hashed"))?;
        if request.spec.expires_at <= now {
            return Err(Refusal::Invalid("the request window has already ended"));
        }
        let row = RequestRow {
            id: request.id.clone(),
            request: request_json_text.to_owned(),
            request_hash: to_hex(&hash),
            status: RequestStatus::Pending,
            requested_at: now,
            expires_at: request.spec.expires_at,
        };
        self.audit_must(
            "support.request_received",
            &row.id,
            &request.requester.id,
            "received",
            now,
        )
        .await?;
        if !self
            .store
            .put_request(&row, now)
            .await
            .map_err(from_store)?
        {
            return Err(Refusal::Invalid("a request with this id already exists"));
        }
        Ok(row.id)
    }

    /// Lists the tenant's requests (newest ids last), at most `limit`, after `after`.
    ///
    /// # Errors
    /// [`Refusal::Denied`] unless a security administrator; [`Refusal::Unavailable`].
    pub async fn list_requests(
        &self,
        caller: &Option<CallerInfo>,
        after: Option<&str>,
        limit: u32,
        now: i64,
    ) -> Result<Vec<serde_json::Value>, Refusal> {
        let caller = self
            .gate(caller, "support.request_list", "-", false, now)
            .await?;
        let rows = self
            .store
            .list_requests(None, after, limit)
            .await
            .map_err(from_store)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let approvals = self
                .store
                .consents(&row.id)
                .await
                .map_err(from_store)?
                .len();
            let needed =
                serde_json::from_str::<AccessRequest>(&row.request).map_or(2, |r| self.needed(&r));
            out.push(request_json(row, approvals, needed));
        }
        self.audit("support.request_list", "-", &caller.user_id, "ok", now)
            .await;
        Ok(out)
    }

    async fn trust(&self) -> Result<CustomerTrust, Refusal> {
        let mut trust = CustomerTrust::default();
        for key in self.store.keys().await.map_err(from_store)? {
            trust.add(
                &self.tenant,
                TrustedApprover {
                    identity: key.identity,
                    public_key: key.public_key,
                    is_owner: key.is_owner,
                },
            );
        }
        Ok(trust)
    }

    /// The customer approves a request with a signed consent. Verified here against the enrolled
    /// key of the **caller's own identity**; the nonce is not consumed (issuance does that).
    ///
    /// # Errors
    /// [`Refusal::Denied`], [`Refusal::StepUp`], [`Refusal::NotFound`], [`Refusal::NotPending`],
    /// [`Refusal::ConsentInvalid`] with the `deep_access.*` code, [`Refusal::Unavailable`].
    pub async fn consent(
        &self,
        caller: &Option<CallerInfo>,
        request_id: &str,
        record_text: &str,
        signature_b64: &str,
        now: i64,
    ) -> Result<serde_json::Value, Refusal> {
        let caller = self
            .gate(caller, "support.consent", request_id, true, now)
            .await?;
        let Some(row) = self
            .store
            .get_request(request_id)
            .await
            .map_err(from_store)?
        else {
            self.audit(
                "support.consent",
                request_id,
                &caller.user_id,
                "not_found",
                now,
            )
            .await;
            return Err(Refusal::NotFound);
        };
        if row.status != RequestStatus::Pending || row.expires_at <= now {
            self.audit(
                "support.consent",
                request_id,
                &caller.user_id,
                "not_pending",
                now,
            )
            .await;
            return Err(Refusal::NotPending);
        }
        let refuse = |code: &'static str| Refusal::ConsentInvalid(code);
        let request: AccessRequest =
            serde_json::from_str(&row.request).map_err(|_| Refusal::Unavailable)?;
        let signature = super::base64_decode(signature_b64, 1024)
            .ok_or_else(|| refuse("deep_access.bad_signature"))?;
        let outcome: Result<_, &'static str> = (|| {
            let record = ConsentRecord::from_json(record_text).map_err(|e| e.code())?;
            let receipt = record.into_receipt(signature).map_err(|e| e.code())?;
            Ok(receipt)
        })();
        let receipt = match outcome {
            Ok(receipt) => receipt,
            Err(code) => {
                self.audit(
                    "support.consent",
                    request_id,
                    &caller.user_id,
                    "malformed",
                    now,
                )
                .await;
                return Err(refuse(code));
            }
        };
        // The approver is the signed-in person: a consent cannot be submitted on someone else's
        // behalf, and the signer's identity is the caller's.
        if receipt.approver != caller.user_id {
            self.audit(
                "support.consent",
                request_id,
                &caller.user_id,
                "approver_mismatch",
                now,
            )
            .await;
            return Err(refuse("deep_access.unknown_approver"));
        }
        let trust = self.trust().await?;
        if let Err(error) = check_consent(&request, &receipt, &trust, None, now) {
            self.audit(
                "support.consent",
                request_id,
                &caller.user_id,
                error.code(),
                now,
            )
            .await;
            return Err(refuse(error.code()));
        }
        self.audit_must(
            "support.consent",
            request_id,
            &caller.user_id,
            "approved",
            now,
        )
        .await?;
        let receipt_hash = receipt.hash().map_err(|_| Refusal::Unavailable)?;
        let consent = ConsentRow {
            request_id: request_id.to_owned(),
            approver: caller.user_id.clone(),
            approver_key: receipt.approver_key.as_str().to_owned(),
            record: record_text.to_owned(),
            signature: receipt.signature.clone(),
            receipt_hash: to_hex(&receipt_hash),
            recorded_at: now,
        };
        if !self
            .store
            .record_consent(&consent)
            .await
            .map_err(from_store)?
        {
            // The same approver again is not a second approval.
            return Err(refuse("deep_access.operator_approval"));
        }
        let approvals = self
            .store
            .consents(request_id)
            .await
            .map_err(from_store)?
            .len();
        let needed = self.needed(&request);
        let approved = approvals >= needed;
        if approved {
            self.store
                .transition(
                    request_id,
                    &[RequestStatus::Pending],
                    RequestStatus::Approved,
                    now,
                )
                .await
                .map_err(from_store)?;
        }
        Ok(serde_json::json!({
            "status": if approved { "approved" } else { "pending" },
            "approvals": approvals,
            "approvals_needed": needed,
        }))
    }

    /// The customer revokes a request and, at once, every session issued from it. Effective on
    /// the next `authorise` (nothing is cached).
    ///
    /// # Errors
    /// [`Refusal::Denied`], [`Refusal::StepUp`], [`Refusal::NotFound`], [`Refusal::NotPending`]
    /// for a request already revoked or rejected, [`Refusal::Unavailable`].
    pub async fn revoke(
        &self,
        caller: &Option<CallerInfo>,
        request_id: &str,
        reason: &str,
        now: i64,
    ) -> Result<serde_json::Value, Refusal> {
        let caller = self
            .gate(caller, "support.revoke", request_id, true, now)
            .await?;
        let Some(row) = self
            .store
            .get_request(request_id)
            .await
            .map_err(from_store)?
        else {
            self.audit(
                "support.revoke",
                request_id,
                &caller.user_id,
                "not_found",
                now,
            )
            .await;
            return Err(Refusal::NotFound);
        };
        // The audit record comes first: a revocation whose record cannot be written is still
        // carried out below only after the record exists.
        self.audit_must(
            "support.revoke",
            request_id,
            &caller.user_id,
            "revoked",
            now,
        )
        .await?;
        let moved = self
            .store
            .transition(
                request_id,
                &[
                    RequestStatus::Pending,
                    RequestStatus::Approved,
                    RequestStatus::Issued,
                ],
                RequestStatus::Revoked,
                now,
            )
            .await
            .map_err(from_store)?;
        if !moved && row.status != RequestStatus::Expired {
            return Err(Refusal::NotPending);
        }
        let mut ended = 0_usize;
        for (credential, _) in self.store.credentials(100).await.map_err(from_store)? {
            if credential.request_id == request_id {
                self.store
                    .revoke(credential.session_id, &caller.user_id, reason, now)
                    .await
                    .map_err(from_store)?;
                ended += 1;
            }
        }
        Ok(serde_json::json!({"status": "revoked", "sessions_ended": ended}))
    }

    /// The tenant's sessions: active, past and revoked, as the customer sees them.
    ///
    /// # Errors
    /// [`Refusal::Denied`], [`Refusal::Unavailable`].
    pub async fn list_sessions(
        &self,
        caller: &Option<CallerInfo>,
        limit: u32,
        now: i64,
    ) -> Result<Vec<serde_json::Value>, Refusal> {
        let caller = self
            .gate(caller, "support.session_list", "-", false, now)
            .await?;
        let rows = self.store.credentials(limit).await.map_err(from_store)?;
        self.audit("support.session_list", "-", &caller.user_id, "ok", now)
            .await;
        Ok(rows
            .iter()
            .map(|(c, revoked)| {
                let state = if *revoked {
                    "revoked"
                } else if now >= c.expires_at {
                    "expired"
                } else if now < c.starts_at {
                    "scheduled"
                } else {
                    "active"
                };
                serde_json::json!({
                    "id": to_hex(&c.session_id),
                    "request_id": c.request_id,
                    "scope": c.scope,
                    "starts_at": c.starts_at,
                    "expires_at": c.expires_at,
                    "state": state,
                })
            })
            .collect())
    }

    /// The actions taken under one session, from the tenant audit chain: exactly `at`, `action`
    /// and `target`. Never anything the operator could edit.
    ///
    /// # Errors
    /// [`Refusal::Denied`], [`Refusal::StepUp`], [`Refusal::Invalid`] for a malformed id,
    /// [`Refusal::NotFound`], [`Refusal::Unavailable`].
    pub async fn recording(
        &self,
        caller: &Option<CallerInfo>,
        session_hex: &str,
        limit: u32,
        now: i64,
    ) -> Result<Vec<serde_json::Value>, Refusal> {
        let caller = self
            .gate(caller, "support.recording", session_hex, true, now)
            .await?;
        if session_hex.len() != 32
            || !session_hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Refusal::Invalid(
                "a session id is 32 lower-case hex characters",
            ));
        }
        let known = self
            .store
            .credentials(100)
            .await
            .map_err(from_store)?
            .iter()
            .any(|(c, _)| to_hex(&c.session_id) == session_hex);
        if !known {
            self.audit(
                "support.recording",
                session_hex,
                &caller.user_id,
                "not_found",
                now,
            )
            .await;
            return Err(Refusal::NotFound);
        }
        let Some(recordings) = &self.recordings else {
            return Err(Refusal::Unavailable);
        };
        self.audit_must(
            "support.recording",
            session_hex,
            &caller.user_id,
            "read",
            now,
        )
        .await?;
        let rows = recordings
            .actions(session_hex, limit.clamp(1, 100))
            .await
            .map_err(|_| Refusal::Unavailable)?;
        Ok(rows
            .into_iter()
            .map(|r| serde_json::json!({"at": r.at, "action": r.action, "target": r.target}))
            .collect())
    }
}
