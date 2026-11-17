//! Invitations (WP-IAM-12.T5): the mail of an invitation, queued in the same transaction as the
//! invitation itself, and accepting an invitation through the public, throttled route
//! `POST /api/v1/auth/invitations/accept` (`auth_invitation_accept`).
//!
//! # What an invitation is
//!
//! A record of the platform DocType `invitation` (`apps/platform/doctypes/invitation.toml`:
//! `user`, `email`, `state`, `expires_at`, `sent_at`, `accepted_at`, `digest`). The state machine
//! is `pending`, then exactly one of `accepted`, `expired` (the clock) or `revoked`
//! (an administrator, or a disable of the user); nothing leaves a final state. The token is 256
//! random bits shown once, in the mail; the record keeps only the lowercase hex of its SHA-256
//! digest (a digest of that much entropy cannot be searched for, so the column is an ordinary
//! unique text and not a sealed secret).
//!
//! # The mail is part of the transaction
//!
//! [`issue_with_mail`] writes the invitation and enqueues the `platform:send_mail` job for it as an
//! outbox event of the **same transaction**: both are committed or neither is. A kill between the
//! two cannot leave an invitation nobody was told about (a pending record with no mail) or a mail
//! about an invitation that does not exist. [`InvitationMailer`] is the adapter for the
//! after-commit port `identity_admin::InvitationMail` that WP-IAM-12.T2 calls: it enqueues the
//! mail of an invitation that is already committed (in a transaction of its own, found by
//! the token's digest), so until the user administration calls [`issue_with_mail`] itself a
//! failure still leaves the invitation pending and the resend retries.
//!
//! The mail job carries the message, and the message carries the token: that is the only place a
//! token exists besides the mail itself, until the job completes. The job and the outbox are the
//! platform's own tables, the token is never logged, and a token is useless once the invitation
//! is accepted, expired or revoked.
//!
//! # Accepting
//!
//! [`accept_in`] runs in one transaction. It looks the token up by its digest, locks the
//! invitation row, and refuses with the **one** answer `identity.invitation_invalid` for an
//! unknown, malformed, used, revoked, expired or inapplicable token (the expiry is judged by the
//! injected clock; an expired pending invitation is marked `expired`). A password that fails the
//! tenant's policy is a different answer (`auth.password_policy`) that does not use the token up;
//! the policy is judged only after the token proved valid, so the answer tells nothing to someone
//! without one. The password verifier is stored through the [`InvitationCredentials`] port, the
//! user becomes `active` and the invitation `accepted` in the same transaction; the roles of the
//! user are exactly the assignments the administrator made when inviting (nothing is added here).
//! Two accepts of one token serialise on the invitation row: one wins, the other gets the one
//! refusal.
//!
//! The route is public and runs behind the per-client-address rate limit of the `auth` class
//! (the registry) and behind the site's open-path limit
//! (`[services.invitations] accept_rate_per_minute`); every accept, success or refusal, is audited
//! on `spot::audit::identity` (what, outcome, the user on success; never the token, the password
//! or the address).
//!
//! Settings `[services.invitations]`: `enabled` (true), `accept_rate_per_minute` (60, 1 to 6000).
//! Level: optional.

use std::sync::Arc;

use axum::http::Method;
use serde::Deserialize;
use serde_json::{Value as Json, json};
use spot_auth::{HashParams, OpaqueToken, Password, PasswordHash, PasswordPolicy, hash_password};
use spot_core::{Clock, SiteId};
use spot_document::engine::Engine;
use spot_events_contract::payload::{EventMeta, PayloadDocument};
use spot_events_contract::{Actor, Topic};
use spot_http::api::Ports;
use spot_http::api::ports::invitation_accept::{AcceptError, AcceptIn, InvitationAcceptPort};
use spot_http::api::router::routes::{self, Registration};
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_mail_queue::{MAIL_JOB_NAME, QueuedMailbox, QueuedMessage, enqueue_mail};
use spot_meta::Definition;
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, DocumentData, Isolation, Name, NodeRef, OutboxEvent, RecordId, RowLock,
    StorageBackend, Transaction, TxOptions, Value, Wait,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::live::SiteLive;
use crate::services::identity_admin::InvitationMail;
use crate::services::scim_store::{at, blank, doc, dt, find_id, hex, idv, nm, put, text};
use crate::services::{Idle, Level, OpenPath, RateLimit, SiteCtx};
use crate::{Error, Result};

/// After the mail module (700) and the sign-in service, before the user administration (1050),
/// which looks for the [`InvitationMail`] port this module provides.
pub const ORDER: u32 = 1040;

/// The audit target shared with the rest of identity administration.
const AUDIT: &str = "spot::audit::identity";
/// The DocType.
pub const INVITATION: &str = "invitation";
/// The public path (a contract path: the host's open-path table exempts it, see the report).
pub const ACCEPT_PATH: &str = "/api/v1/auth/invitations/accept";
/// The component that enqueues the mail.
const COMPONENT: &str = "invitations";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    enabled: bool,
    accept_rate_per_minute: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            accept_rate_per_minute: 60,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------------------------

/// Stores a user's password verifier. Provided by the sign-in service (the host adapter over its
/// credential store); accepting an invitation is unavailable without it. The verifier is a
/// one-way hash: the port never sees the password.
pub trait InvitationCredentials: Send + Sync {
    /// Sets `user`'s password verifier. It must not change anything else about the user.
    fn set_password<'a>(
        &'a self,
        user: RecordId,
        hash: PasswordHash,
    ) -> BoxFuture<'a, std::result::Result<(), String>>;
}

/// Why a step of issuing or accepting did not happen. No text: a storage error can name a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unavailable;

fn storage<T, E>(r: std::result::Result<T, E>) -> std::result::Result<T, Unavailable> {
    r.map_err(|_| Unavailable)
}

// ---------------------------------------------------------------------------------------------
// Tokens and states
// ---------------------------------------------------------------------------------------------

/// The lowercase hex of the digest of `token`, or `None` for text that cannot be a token.
#[must_use]
pub fn token_digest_hex(token: &str) -> Option<String> {
    OpaqueToken::parse(token)
        .ok()
        .map(|t| hex(t.digest().as_bytes()))
}

/// What the clock says about an invitation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Pending and not yet expired: it may be accepted.
    Accept,
    /// Pending but past its expiry: it becomes `expired` and the token is refused.
    Expired,
    /// Accepted, expired, revoked or unknown state: refused.
    Refuse,
}

/// The decision for an invitation in `state` that expires at `expires_at`, at `now`. A missing
/// expiry is a refusal (fail closed).
#[must_use]
pub fn decide(state: &str, expires_at: Option<OffsetDateTime>, now: OffsetDateTime) -> Decision {
    match (state, expires_at) {
        ("pending", Some(expires)) if now < expires => Decision::Accept,
        ("pending", Some(_)) => Decision::Expired,
        _ => Decision::Refuse,
    }
}

// ---------------------------------------------------------------------------------------------
// The mail
// ---------------------------------------------------------------------------------------------

/// What the mail job needs to know about the site.
#[derive(Clone, Debug)]
pub struct MailSpec {
    /// The site the job belongs to.
    pub site: SiteId,
    /// The sending account's address (the account of `services.mail`).
    pub from: String,
}

/// The message of an invitation: the token in the text body only, never in the subject or a
/// header, and the expiry in UTC.
#[must_use]
pub fn invitation_message(
    from: &str,
    to: &str,
    site: &str,
    token: &str,
    expires_at: OffsetDateTime,
) -> QueuedMessage {
    let mut message = QueuedMessage::new(
        QueuedMailbox::address(from),
        format!("Your invitation to {site}"),
    );
    message.to.push(QueuedMailbox::address(to));
    let expires = expires_at.format(&Rfc3339).unwrap_or_default();
    message.text = Some(format!(
        "You have been invited to {site}.\n\n\
         Your invitation token (use it once, and keep it to yourself):\n\n{token}\n\n\
         It expires at {expires}. If you did not expect this message, ignore it: nothing happens \
         unless the token is used.\n"
    ));
    message
}

/// The outbox event that enqueues the mail job of one invitation. The shape is the one the
/// document engine's own job events have (`sys_job_enqueued`: job, args, idempotency key), so the
/// jobs intake takes it like any other. The idempotency key is the message id, so one invitation
/// token is one mail however often the event is replayed.
///
/// # Errors
///
/// [`Unavailable`] when the message does not validate (an address the mail library refuses) or
/// the event cannot be built.
pub fn mail_event(
    spec: &MailSpec,
    invitation: RecordId,
    email: &str,
    token: &str,
    expires_at: OffsetDateTime,
    now: OffsetDateTime,
) -> std::result::Result<OutboxEvent, Unavailable> {
    let digest = token_digest_hex(token).ok_or(Unavailable)?;
    let message = invitation_message(&spec.from, email, spec.site.as_str(), token, expires_at);
    let message_id = format!("invitation-{invitation}-{}", &digest[..16]);
    let job = storage(enqueue_mail(
        spec.site.clone(),
        &message,
        crate::services::mail::ACCOUNT,
        &message_id,
        now,
    ))?;
    debug_assert_eq!(job.name, MAIL_JOB_NAME);
    let id = RecordId::new_v7();
    let meta = EventMeta {
        occurred_at: now,
        actor: Actor::System {
            component: storage(nm(COMPONENT))?,
        },
        command_id: None,
        causation_id: None,
        correlation_id: id,
        trace_id: None,
    };
    let data = json!({
        "job": job.name,
        "args": job.args,
        "idempotency_key": message_id,
    });
    let topic = storage(Topic::any("sys_job_enqueued"))?;
    let subject = NodeRef {
        entity: storage(nm(INVITATION))?,
        id: invitation,
    };
    storage(PayloadDocument { meta, data }.to_outbox_event(id, &topic, subject, None, 1))
}

// ---------------------------------------------------------------------------------------------
// Issuing
// ---------------------------------------------------------------------------------------------

/// A freshly issued invitation. The token is a secret: it is shown here once, to be handed to the
/// mail, and redacted in `Debug`.
pub struct Issued {
    /// The invitation record.
    pub id: RecordId,
    /// The token (a secret).
    pub token: String,
    /// When it expires.
    pub expires_at: OffsetDateTime,
}

impl std::fmt::Debug for Issued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issued")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Writes a `pending` invitation for `user` (inviting `email`) valid for `ttl`, and its mail
/// job, in the caller's transaction: both are committed with it or neither is.
///
/// The caller commits (or rolls back) `tx`; nothing is committed here. Cancelling the user's
/// earlier invitations (a resend) is the caller's, as it was in the user administration.
///
/// # Errors
///
/// [`Unavailable`] for any storage or validation failure; nothing was durably written.
pub async fn issue_with_mail<T: Transaction>(
    tx: &mut T,
    defs: &[Definition],
    spec: &MailSpec,
    user: RecordId,
    email: &str,
    now: OffsetDateTime,
    ttl: time::Duration,
) -> std::result::Result<Issued, Unavailable> {
    let token = OpaqueToken::generate();
    let digest = hex(token.digest().as_bytes());
    let expires_at = now + ttl;
    let mut fields = storage(blank(defs, INVITATION))?;
    storage(put(
        &mut fields,
        vec![
            ("user", Value::Id(user)),
            ("email", email.into()),
            ("state", "pending".into()),
            ("expires_at", dt(expires_at)),
            ("sent_at", dt(now)),
            ("digest", digest.as_str().into()),
        ],
    ))?;
    let id = RecordId::new_v7();
    let record = storage(doc(INVITATION, id, fields))?;
    storage(tx.insert(&record).await)?;
    let event = mail_event(spec, id, email, token.as_str(), expires_at, now)?;
    storage(tx.enqueue(&event).await)?;
    Ok(Issued {
        id,
        token: token.as_str().to_owned(),
        expires_at,
    })
}

/// Enqueues the mail of an invitation that is already committed, in the caller's transaction:
/// the invitation is found by the token's digest. For the after-commit port of the user
/// administration.
///
/// # Errors
///
/// [`Unavailable`] when no invitation holds this token, or the event cannot be written.
pub async fn enqueue_for_committed<T: Transaction>(
    tx: &mut T,
    spec: &MailSpec,
    email: &str,
    token: &str,
    expires_at: OffsetDateTime,
    now: OffsetDateTime,
) -> std::result::Result<RecordId, Unavailable> {
    let digest = token_digest_hex(token).ok_or(Unavailable)?;
    let id = storage(find_id(tx, INVITATION, "digest", &digest).await)?.ok_or(Unavailable)?;
    let event = mail_event(spec, id, email, token, expires_at, now)?;
    storage(tx.enqueue(&event).await)?;
    Ok(id)
}

// ---------------------------------------------------------------------------------------------
// Accepting
// ---------------------------------------------------------------------------------------------

/// What accepting did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The password is set, the user is active, the invitation is accepted. Commit.
    Accepted(RecordId),
    /// The invitation was pending past its expiry and is now marked `expired`. Commit; the
    /// answer is the one refusal.
    Expired,
    /// Refused: the one answer for every bad token. Roll back.
    Invalid,
    /// The token is good, the password failed the tenant's policy. Roll back; the token is
    /// still usable.
    PasswordPolicy,
    /// A store or the credential port failed. Roll back; nothing changed.
    Unavailable,
}

impl Verdict {
    /// Whether the transaction is committed (otherwise rolled back).
    #[must_use]
    pub fn commits(self) -> bool {
        matches!(self, Verdict::Accepted(_) | Verdict::Expired)
    }

    /// The answer the route gives.
    pub fn answer(self) -> std::result::Result<(), AcceptError> {
        match self {
            Verdict::Accepted(_) => Ok(()),
            Verdict::Expired | Verdict::Invalid => Err(AcceptError::Invalid),
            Verdict::PasswordPolicy => Err(AcceptError::PasswordPolicy),
            Verdict::Unavailable => Err(AcceptError::Unavailable),
        }
    }

    /// A short, fixed word for the audit line (never a value).
    #[must_use]
    pub fn outcome(self) -> &'static str {
        match self {
            Verdict::Accepted(_) => "ok",
            Verdict::Expired => "refused_expired",
            Verdict::Invalid => "refused",
            Verdict::PasswordPolicy => "refused_password_policy",
            Verdict::Unavailable => "unavailable",
        }
    }
}

/// What accepting needs besides the transaction.
pub struct AcceptEnv<'a> {
    /// The site (a context word of the password policy).
    pub site: &'a str,
    /// The injected clock's now.
    pub now: OffsetDateTime,
    /// Where the password verifier goes.
    pub credentials: &'a dyn InvitationCredentials,
    /// The tenant's password policy.
    pub policy: &'a PasswordPolicy,
    /// The hash cost (the platform default; tests use cheap parameters).
    pub params: &'a HashParams,
}

fn field(data: &DocumentData, name: &str) -> Value {
    Name::new(name)
        .ok()
        .and_then(|n| data.fields.get(&n).cloned())
        .unwrap_or(Value::Null)
}

fn set(data: &mut DocumentData, pairs: Vec<(&str, Value)>) -> std::result::Result<(), Unavailable> {
    for (name, value) in pairs {
        data.fields.insert(storage(nm(name))?, value);
    }
    Ok(())
}

async fn identity_event<T: Transaction>(
    tx: &mut T,
    now: OffsetDateTime,
    by: RecordId,
    topic: &str,
    user: RecordId,
    data: Json,
) -> std::result::Result<(), Unavailable> {
    let id = RecordId::new_v7();
    let meta = EventMeta {
        occurred_at: now,
        actor: Actor::User { id: by, via: None },
        command_id: None,
        causation_id: None,
        correlation_id: id,
        trace_id: None,
    };
    let topic = storage(Topic::any(topic))?;
    let subject = NodeRef {
        entity: storage(nm(doctype::USER))?,
        id: user,
    };
    let event =
        storage(PayloadDocument { meta, data }.to_outbox_event(id, &topic, subject, None, 1))?;
    storage(tx.enqueue(&event).await)
}

/// Accepts the invitation `input.token` names, in `tx` (see the module documentation for the
/// order of the steps). The caller commits when [`Verdict::commits`], rolls back otherwise.
pub async fn accept_in<T: Transaction>(
    tx: &mut T,
    env: &AcceptEnv<'_>,
    input: &AcceptIn,
) -> Verdict {
    match accept_steps(tx, env, input).await {
        Ok(verdict) => verdict,
        Err(Unavailable) => Verdict::Unavailable,
    }
}

async fn accept_steps<T: Transaction>(
    tx: &mut T,
    env: &AcceptEnv<'_>,
    input: &AcceptIn,
) -> std::result::Result<Verdict, Unavailable> {
    // 1. The token, by its digest. Anything that cannot be a token is the one refusal.
    let Some(digest) = token_digest_hex(&input.token) else {
        return Ok(Verdict::Invalid);
    };
    let Some(id) = storage(find_id(tx, INVITATION, "digest", &digest).await)? else {
        return Ok(Verdict::Invalid);
    };
    // 2. The invitation row, locked: a second accept of the same token waits here and then
    //    sees the final state.
    let entity = storage(nm(INVITATION))?;
    let Some(invitation) = storage(tx.load(&entity, id, RowLock::Update(Wait::Block)).await)?
    else {
        return Ok(Verdict::Invalid);
    };
    let state = text(&field(&invitation.data, "state")).unwrap_or_default();
    let expires_at = at(&field(&invitation.data, "expires_at"));
    let decision = decide(&state, expires_at, env.now);
    if decision == Decision::Refuse {
        return Ok(Verdict::Invalid);
    }
    if decision == Decision::Expired {
        let mut data = invitation.data;
        set(&mut data, vec![("state", "expired".into())])?;
        storage(tx.update(&data, invitation.version).await)?;
        return Ok(Verdict::Expired);
    }
    let Some(user) = idv(&field(&invitation.data, "user")) else {
        return Ok(Verdict::Invalid);
    };
    // 3. The user: only an invited user can accept (a disabled or already active account is
    //    not given a new password by an old token). Read without a lock: the credential port
    //    writes the same row in a transaction of its own.
    let user_entity = storage(nm(doctype::USER))?;
    let Some(person) = storage(tx.load(&user_entity, user, RowLock::None).await)? else {
        return Ok(Verdict::Invalid);
    };
    if text(&field(&person.data, "state")).as_deref() != Some("invited") {
        return Ok(Verdict::Invalid);
    }
    // 4. The password policy, after the token is known to be good, with the words the policy
    //    forbids: the address, its local part and the site.
    let email = text(&field(&person.data, "email")).unwrap_or_default();
    let local = email.split('@').next().unwrap_or("").to_owned();
    let password = Password::new(&input.password);
    if env
        .policy
        .check(&password, &[email.as_str(), local.as_str(), env.site])
        .is_err()
    {
        return Ok(Verdict::PasswordPolicy);
    }
    // 5. The verifier. The hash is computed here (the one place the password is read) and only
    //    the hash crosses the port.
    let hash = hash_password(&password, env.params);
    if env.credentials.set_password(user, hash).await.is_err() {
        return Ok(Verdict::Unavailable);
    }
    // 6. The user becomes active (re-read with the lock: the credential port just wrote the
    //    row), and the invitation is accepted.
    let Some(person) = storage(
        tx.load(&user_entity, user, RowLock::Update(Wait::Block))
            .await,
    )?
    else {
        return Ok(Verdict::Unavailable);
    };
    let mut data = person.data;
    let mut changes = vec![("state", Value::from("active"))];
    if let Some(name) = input.full_name.as_deref() {
        changes.push(("full_name", name.into()));
    }
    set(&mut data, changes)?;
    storage(tx.update(&data, person.version).await)?;
    let mut data = invitation.data;
    set(
        &mut data,
        vec![("state", "accepted".into()), ("accepted_at", dt(env.now))],
    )?;
    storage(tx.update(&data, invitation.version).await)?;
    identity_event(
        tx,
        env.now,
        user,
        "identity_user_enabled",
        user,
        json!({"user": user.to_string(), "reason": "invitation_accepted"}),
    )
    .await?;
    identity_event(
        tx,
        env.now,
        user,
        "identity_password_changed",
        user,
        json!({"user": user.to_string(), "by": "user", "sessions_ended": 0}),
    )
    .await?;
    Ok(Verdict::Accepted(user))
}

// ---------------------------------------------------------------------------------------------
// The hosted services
// ---------------------------------------------------------------------------------------------

type Tx = <PgBackend as StorageBackend>::Tx;

async fn begin(
    live: &SiteLive,
    catalog: &Arc<Catalog>,
    label: &str,
) -> std::result::Result<Tx, Unavailable> {
    let engine: Arc<Engine<PgBackend>> = live.engine().ok_or(Unavailable)?;
    // Read committed with the explicit row lock: a waiting accept re-reads the row once the
    // first one committed.
    let options = TxOptions::new(storage(nm(label))?, Isolation::ReadCommitted);
    storage(engine.backend().begin(Arc::clone(catalog), options).await)
}

/// The mail adapter for the user administration's after-commit port.
pub struct InvitationMailer {
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    catalog: Arc<Catalog>,
    spec: MailSpec,
}

impl InvitationMail for InvitationMailer {
    fn send<'a>(
        &'a self,
        address: &'a str,
        token: &'a str,
        expires_at: OffsetDateTime,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            let now = self.clock.now_utc();
            let mut tx = begin(&self.live, &self.catalog, "invitation_mail")
                .await
                .map_err(|_| "the mail queue is unavailable".to_owned())?;
            match enqueue_for_committed(&mut tx, &self.spec, address, token, expires_at, now).await
            {
                Ok(_) => tx
                    .commit()
                    .await
                    .map_err(|_| "the mail queue is unavailable".to_owned()),
                Err(Unavailable) => {
                    let _ = tx.rollback().await;
                    Err("the invitation mail could not be queued".to_owned())
                }
            }
        })
    }
}

/// The public acceptance of invitations of one site.
pub struct InvitationAccept {
    site: String,
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    catalog: Arc<Catalog>,
    credentials: Arc<dyn InvitationCredentials>,
    policy: PasswordPolicy,
    params: HashParams,
}

impl InvitationAccept {
    fn audit(&self, outcome: &str, user: Option<RecordId>) {
        tracing::info!(
            target: AUDIT,
            site = %self.site,
            op = "invitation_accept",
            target_user = user.map(|u| u.to_string()).as_deref().unwrap_or(""),
            outcome,
            "invitation acceptance"
        );
    }
}

impl InvitationAcceptPort for InvitationAccept {
    fn accept<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        input: AcceptIn,
    ) -> BoxFuture<'a, std::result::Result<(), AcceptError>> {
        Box::pin(async move {
            // A site is its own tenant: a request for another site is unknown here.
            if ctx.site.as_str() != self.site {
                self.audit("refused", None);
                return Err(AcceptError::Invalid);
            }
            let Ok(mut tx) = begin(&self.live, &self.catalog, "invitation_accept").await else {
                self.audit("unavailable", None);
                return Err(AcceptError::Unavailable);
            };
            let env = AcceptEnv {
                site: &self.site,
                now: self.clock.now_utc(),
                credentials: self.credentials.as_ref(),
                policy: &self.policy,
                params: &self.params,
            };
            let verdict = accept_in(&mut tx, &env, &input).await;
            let verdict = if verdict.commits() {
                match tx.commit().await {
                    Ok(()) => verdict,
                    Err(_) => Verdict::Unavailable,
                }
            } else {
                let _ = tx.rollback().await;
                verdict
            };
            // A success that cannot be recorded is not reported as one: the audit line is a
            // tracing event, which cannot fail; the durable record is the identity events
            // written in the transaction.
            let user = match verdict {
                Verdict::Accepted(user) => Some(user),
                _ => None,
            };
            self.audit(verdict.outcome(), user);
            verdict.answer()
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------------------------

/// Joins the site unless `[services.invitations] enabled = false`.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("invitations")?;
    if !settings.enabled {
        return Ok(());
    }
    if !(1..=6000).contains(&settings.accept_rate_per_minute) {
        return Err(Error(
            "services.invitations: accept_rate_per_minute is 1 to 6000".to_owned(),
        ));
    }
    let (site, live, catalog, mail) = {
        let env = ctx.env()?;
        (
            env.site.site.clone(),
            Arc::clone(env.live),
            env.permission.catalog.clone(),
            env.loaded.common.services.mail.clone(),
        )
    };
    let defs: Vec<Definition> = spot_platform_app::definitions().map_err(crate::error)?;
    let has_doc = defs.iter().any(|d| d.name == INVITATION);
    let credentials = ctx.port::<dyn InvitationCredentials>();
    let mail_on = mail.enabled;
    if ctx.assembling() {
        let (doc_ok, cred_ok) = (has_doc, credentials.is_some());
        ctx.add(Idle::new("invitations", Level::Optional, move || {
            Box::pin(async move {
                match (doc_ok, mail_on, cred_ok) {
                    (true, true, true) => Ok(()),
                    (false, _, _) => Err("the platform app has no `invitation` DocType: invitations are off".to_owned()),
                    (_, false, _) => Err("mail is disabled: invitation mail is not provided".to_owned()),
                    (_, _, false) => Err("no sign-in service provides the password store: invitations cannot be accepted".to_owned()),
                }
            })
        }));
    }
    if !has_doc {
        return Ok(());
    }
    let clock = Arc::clone(ctx.clock());
    let from = mail.smtp.as_ref().map_or_else(
        || "noreply@localhost.invalid".to_owned(),
        |s| s.from.clone(),
    );
    if mail_on {
        let mailer: Arc<dyn InvitationMail> = Arc::new(InvitationMailer {
            live: Arc::clone(&live),
            clock: Arc::clone(&clock),
            catalog: Arc::clone(&catalog),
            spec: MailSpec {
                site: SiteId::new(&site).map_err(crate::error)?,
                from,
            },
        });
        ctx.provide(mailer)?;
    }
    let Some(credentials) = credentials else {
        return Ok(());
    };
    let accept: Arc<dyn InvitationAcceptPort> = Arc::new(InvitationAccept {
        site,
        live,
        clock,
        catalog,
        credentials,
        policy: PasswordPolicy::default(),
        params: HashParams::default(),
    });
    // The route is reachable without a session: the host's open-path table must list it (a
    // refusal by the host, for the reserved `/api/v1/auth/` prefix, leaves the route mounted
    // but unreachable, and is reported on readiness rather than stopping the site).
    let open = ctx.open_paths(&[OpenPath {
        method: Method::POST,
        pattern: ACCEPT_PATH,
        limit: RateLimit {
            per_minute: settings.accept_rate_per_minute,
        },
    }]);
    if let Err(error) = open {
        tracing::warn!(
            error = %error.0,
            "the invitation acceptance route is not reachable without a session: the host did not accept the open path"
        );
    }
    let mut ports = Ports::new();
    ports.provide::<dyn InvitationAcceptPort>(accept);
    let registration = Registration {
        name: "invitation_accept",
        serves: routes::invitation_accept::SERVES,
        required_ports: routes::invitation_accept::required_ports,
        routes: routes::invitation_accept::routes,
        public: Some(routes::invitation_accept::PUBLIC),
    };
    let mounted = routes::mount(&[registration], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the invitation acceptance route was not mounted"
        );
    }
    let public: Vec<_> = mounted
        .public
        .into_iter()
        .map(|(_, router)| router)
        .collect();
    ctx.router_layer(move |base| {
        public
            .into_iter()
            .fold(base, |acc, router| acc.merge(router))
    });
    Ok(())
}
