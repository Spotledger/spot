//! User administration over HTTP (WP-IAM-12.T2): the user part of `IdentityAdminPort`
//! (`/api/v1/identity/users*` and `/invitations*`) over `spot-iam-directory`'s pure rules.
//!
//! - **One transaction per operation**, as the system principal for the directory writes (the
//!   direct-to-storage path `services/scim_store.rs` uses). A disable writes the state change, ends
//!   every live session, revokes the owner's API keys, cancels pending invitations and queues the
//!   identity events at commit, all together; a refusal rolls everything back.
//! - **Last administrator**: the administrators' rows are locked in id order and counted in the same
//!   transaction as the change, so two concurrent disables of the last two administrators cannot both
//!   succeed (`check_admin_invariant`).
//! - **The caller** is resolved by the site's sign-in service. `identity.*` permissions come from a
//!   fixed table of the caller's active role names (`Tenant Administrator`: all but impersonation;
//!   `User Manager`: read, manage). Any failure to decide is a refusal. A cross-tenant id cannot
//!   exist (one database per site) and an unknown id is `identity.not_found`.
//! - **Step-up** is "the session was created within `step_up_window_secs`" (no step-up marker exists).
//! - **Invitations** are records of a DocType `invitation` (fields `user`, `email`, `state`,
//!   `expires_at`, `sent_at`, `digest`: hex of the token's digest). The platform app has no such
//!   DocType yet: without it the invitation operations and `invite: true` answer `503` and
//!   nothing is written. The token is handed to the [`InvitationMail`] port after commit; it is never
//!   stored, returned or logged.
//! - **Composition**: this module provides `dyn UserAdmin` and mounts the group in a finaliser over
//!   `IdentityAdmin`, which adds whatever other modules provided (`dyn RoleAdmin`, `dyn AssignmentAdmin`,
//!   `dyn KeyAdmin`, `dyn SessionAdmin`); an area nobody provided answers `501`.
//!
//! Settings `[services.identity_admin]`: `enabled` (true), `invitation_ttl_hours` (72, 1 to 720),
//! `step_up_window_secs` (900, 30 to 86400). Level: optional. Audit target `spot::audit::identity`.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value as Json, json};
use spot_core::Clock;
use spot_document::engine::Engine;
use spot_events_contract::payload::{EventMeta, PayloadDocument};
use spot_events_contract::{Actor, Topic};
use spot_http::api::Ports;
use spot_http::api::ports::identity::{
    AssignmentAdmin, AuditEntry, CallerAudience, IdResult, IdentityAdminPort, IdentityAuditPort,
    IdentityCaller, IdentityPermission, IdentityPermissionPort, IdentityStepUpPort, InvitationOut,
    KeyAdmin, Page, PageIn, RecoverOut, RoleAdmin, SessionAdmin, UserAdmin, UserChangeOut,
    UserCreateIn, UserListIn, UserOut, UserPatchIn,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes::{self, Registration};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_iam_directory::{
    Change, ChangeKind, DirectoryView, LeaverAction, RecoveryKind, RecoveryPolicy, RecoveryRequest,
    Requester, Target, User, UserType, check_admin_invariant, required_steps,
};
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, DocumentData, DocumentStore, ErrorKind, Isolation, NodeRef, Outbox, RecordId, RowLock,
    StorageBackend, StorageError, Transaction, TxOptions, Value, Wait,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::live::SiteLive;
use crate::audit::PathClass;
use crate::services::scim_store::{at, blank, doc, dt, find_id, idv, nm, put, rows, text};
use crate::services::{Idle, Level, SiteCtx};
use crate::{Error, Result};

/// After the graph and lineage modules, before the key and session administration (1060).
pub const ORDER: u32 = 1050;

const AUDIT: &str = "spot::audit::identity";
const INVITATION: &str = "invitation";

type Tx = <PgBackend as StorageBackend>::Tx;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    enabled: bool,
    invitation_ttl_hours: u32,
    step_up_window_secs: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            invitation_ttl_hours: 72,
            step_up_window_secs: 900,
        }
    }
}

/// Delivers an invitation token by mail. Provided by a mail module; the token must reach nobody
/// else. Called after the invitation committed; a failure leaves it pending (resend retries).
pub trait InvitationMail: Send + Sync {
    /// Sends the token to `address`.
    fn send<'a>(
        &'a self,
        address: &'a str,
        token: &'a str,
        expires_at: OffsetDateTime,
    ) -> BoxFuture<'a, std::result::Result<(), String>>;
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable)
}

fn not_found() -> ApiError {
    ApiError::from_code(Code::IdentityNotFound)
}

fn invalid(field: &str) -> ApiError {
    ApiError::from_code(Code::IdentityInvalid)
        .detail("fields", Json::Array(vec![Json::from(field)]))
}

fn denied() -> ApiError {
    ApiError::from_code(Code::PermissionDenied)
}

/// A storage failure as the answer. The text is dropped: it can name a value.
fn store(e: &StorageError) -> ApiError {
    match e.kind {
        ErrorKind::UniqueViolation => ApiError::from_code(Code::IdentityUserExists),
        ErrorKind::StaleVersion => ApiError::from_code(Code::DocumentStaleVersion),
        _ => unavailable(),
    }
}

fn infra<T, E>(r: std::result::Result<T, E>) -> IdResult<T> {
    r.map_err(|_| unavailable())
}

fn iam(errors: &spot_iam_directory::IamErrors) -> ApiError {
    let code = errors
        .errors()
        .first()
        .map(|e| e.code)
        .unwrap_or("");
    match code {
        "iam.last_admin" | "iam.break_glass_minimum" => {
            ApiError::from_code(Code::IdentityLastAdministrator)
        }
        _ => ApiError::from_code(Code::IdentityInvalid).detail(
            "fields",
            Json::Array(
                errors
                    .errors()
                    .iter()
                    .filter_map(|e| e.field.map(Json::from))
                    .collect(),
            ),
        ),
    }
}

fn rfc(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

fn record(text: &str) -> IdResult<RecordId> {
    spot_http::api::codec::parse_record_id(text).ok_or_else(not_found)
}

fn kind_of(audience: &str) -> IdResult<UserType> {
    match audience {
        "staff" => Ok(UserType::Internal),
        "partner" => Ok(UserType::Portal),
        _ => Err(invalid("audience")),
    }
}

/// Counts the enabled tenant administrators the transaction read and locked.
struct Admins(BTreeSet<RecordId>);

impl DirectoryView for Admins {
    fn enabled_tenant_admins(&self) -> usize {
        self.0.len()
    }
    fn enabled_break_glass_admins(&self) -> usize {
        0
    }
    fn is_enabled_tenant_admin(&self, user: RecordId) -> bool {
        self.0.contains(&user)
    }
    fn is_enabled_break_glass_admin(&self, _user: RecordId) -> bool {
        false
    }
    fn sso_required(&self) -> bool {
        false
    }
}

const USER_COLS: [&str; 9] = [
    "id",
    "user_name",
    "email",
    "full_name",
    "user_type",
    "state",
    "last_sign_in",
    "manager",
    "deactivated_at",
];

/// A user row as read for the answer.
struct Row {
    id: RecordId,
    name: String,
    email: Option<String>,
    full_name: String,
    user_type: String,
    state: String,
    last_sign_in: Option<OffsetDateTime>,
}

fn row_of(r: &[Value]) -> Option<Row> {
    Some(Row {
        id: idv(&r[0])?,
        name: text(&r[1])?,
        email: text(&r[2]),
        full_name: text(&r[3]).unwrap_or_default(),
        user_type: text(&r[4])?,
        state: text(&r[5])?,
        last_sign_in: at(&r[6]),
    })
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

/// The user administration of one site.
pub struct IdentityUsers {
    site: String,
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    catalog: Arc<Catalog>,
    settings: Settings,
    mail: Option<Arc<dyn InvitationMail>>,
    invitations: bool,
}

impl IdentityUsers {
    fn engine(&self) -> IdResult<Arc<Engine<PgBackend>>> {
        self.live.engine().ok_or_else(unavailable)
    }

    async fn begin(&self, label: &str, read_only: bool) -> IdResult<Tx> {
        let engine = self.engine()?;
        let options = TxOptions::new(infra(nm(label))?, Isolation::Serializable);
        let options = if read_only {
            options.read_only()
        } else {
            options
        };
        engine
            .backend()
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|e| store(&e))
    }

    async fn finish<R>(&self, tx: Tx, result: IdResult<R>) -> IdResult<R> {
        match result {
            Ok(value) => {
                tx.commit().await.map_err(|e| store(&e))?;
                Ok(value)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(e)
            }
        }
    }

    #[allow(dead_code)]
    fn actor(&self, ctx: &RequestCtx, by: RecordId) -> Actor {
        let _ = ctx;
        Actor::User { id: by, via: None }
    }

    /// Writes one identity event about `user` to the outbox.
    async fn event(
        &self,
        tx: &mut Tx,
        topic: &str,
        user: RecordId,
        by: RecordId,
        data: Json,
    ) -> IdResult<()> {
        let id = RecordId::new_v7();
        let meta = EventMeta {
            occurred_at: self.clock.now_utc(),
            actor: Actor::User { id: by, via: None },
            command_id: None,
            causation_id: None,
            correlation_id: id,
            trace_id: None,
        };
        let topic = infra(Topic::any(topic))?;
        let subject = NodeRef {
            entity: infra(nm(doctype::USER))?,
            id: user,
        };
        let event =
            infra(PayloadDocument { meta, data }.to_outbox_event(id, &topic, subject, None, 1))?;
        tx.enqueue(&event).await.map_err(|e| store(&e))
    }

    async fn user_rows(&self, tx: &mut Tx) -> IdResult<Vec<Row>> {
        let all = infra(rows(tx, doctype::USER, &USER_COLS).await)?;
        let mut out: Vec<Row> = all.iter().filter_map(|r| row_of(r)).collect();
        out.sort_by_key(|r| r.id);
        Ok(out)
    }

    /// The role names each user holds through an active assignment.
    async fn roles_of(&self, tx: &mut Tx) -> IdResult<Vec<(RecordId, String)>> {
        let roles = infra(rows(tx, doctype::ROLE, &["id", "role_name"]).await)?;
        let names: Vec<(RecordId, String)> = roles
            .iter()
            .filter_map(|r| Some((idv(&r[0])?, text(&r[1])?)))
            .collect();
        let assigned = infra(rows(tx, doctype::ROLE_ASSIGNMENT, &["user", "role", "state"]).await)?;
        Ok(assigned
            .iter()
            .filter(|r| text(&r[2]).as_deref() == Some("active"))
            .filter_map(|r| {
                let (user, role) = (idv(&r[0])?, idv(&r[1])?);
                let name = names.iter().find(|(id, _)| *id == role)?.1.clone();
                Some((user, name))
            })
            .collect())
    }

    fn out(row: &Row, roles: &[(RecordId, String)], version: u64) -> UserOut {
        UserOut {
            id: row.id.to_string(),
            email: row.email.clone().unwrap_or_else(|| row.name.clone()),
            full_name: row.full_name.clone(),
            audience: if row.user_type == "portal" {
                "partner"
            } else {
                "staff"
            }
            .to_owned(),
            status: row.state.clone(),
            roles: roles
                .iter()
                .filter(|(u, _)| *u == row.id)
                .map(|(_, n)| n.clone())
                .collect(),
            second_factor: false,
            last_sign_in_at: row.last_sign_in.map(rfc),
            created_at: rfc(
                OffsetDateTime::from_unix_timestamp(0).unwrap_or(OffsetDateTime::UNIX_EPOCH)
            ),
            version,
        }
    }

    /// Loads a user document with a write lock, or `identity.not_found`.
    async fn lock_user(
        &self,
        tx: &mut Tx,
        id: RecordId,
    ) -> IdResult<spot_storage_contract::Document> {
        tx.load(&infra(nm(doctype::USER))?, id, RowLock::Update(Wait::Block))
            .await
            .map_err(|e| store(&e))?
            .ok_or_else(not_found)
    }

    async fn view_of(&self, tx: &mut Tx, id: RecordId) -> IdResult<(Row, u64)> {
        let loaded = tx
            .load(&infra(nm(doctype::USER))?, id, RowLock::None)
            .await
            .map_err(|e| store(&e))?
            .ok_or_else(not_found)?;
        let f = |n: &str| nm(n).ok().and_then(|n| loaded.data.fields.get(&n).cloned());
        let get = |n: &str| f(n).unwrap_or(Value::Null);
        let row = row_of(&[
            Value::Id(id),
            get("user_name"),
            get("email"),
            get("full_name"),
            get("user_type"),
            get("state"),
            get("last_sign_in"),
        ])
        .ok_or_else(unavailable)?;
        Ok((row, loaded.version.0))
    }

    /// The enabled tenant administrators, each row locked in id order: the view for the check.
    async fn admins(&self, tx: &mut Tx) -> IdResult<Admins> {
        let roles = self.roles_of(tx).await?;
        let mut ids: Vec<RecordId> = roles
            .iter()
            .filter(|(_, n)| n == spot_platform_app::role::TENANT_ADMINISTRATOR)
            .map(|(u, _)| *u)
            .collect();
        ids.sort();
        ids.dedup();
        let mut live = BTreeSet::new();
        for id in ids {
            let loaded = self.lock_user(tx, id).await?;
            let state = nm("state")
                .ok()
                .and_then(|n| loaded.data.fields.get(&n).and_then(text));
            if state.as_deref() == Some("active") {
                live.insert(id);
            }
        }
        Ok(Admins(live))
    }

    async fn set_state(
        &self,
        tx: &mut Tx,
        id: RecordId,
        state: &str,
        deactivated: Option<OffsetDateTime>,
    ) -> IdResult<u64> {
        let loaded = self.lock_user(tx, id).await?;
        let mut data = loaded.data;
        put_fields(
            &mut data,
            vec![
                ("state", state.into()),
                ("deactivated_at", deactivated.map_or(Value::Null, dt)),
            ],
        )?;
        let version = tx
            .update(&data, loaded.version)
            .await
            .map_err(|e| store(&e))?;
        Ok(version.0)
    }

    /// Ends the user's sessions, revokes their keys and cancels their pending invitations.
    async fn leave(&self, tx: &mut Tx, user: RecordId) -> IdResult<Vec<(String, u64)>> {
        let mut counts = Vec::new();
        for (entity, owner_col, from, to, action) in [
            (doctype::SESSION, "user", "active", "ended", "end_sessions"),
            (
                doctype::API_KEY,
                "owner",
                "active",
                "revoked",
                "disable_api_keys",
            ),
        ] {
            let all = infra(rows(tx, entity, &["id", owner_col, "state"]).await)?;
            let mut n = 0u64;
            for r in all
                .iter()
                .filter(|r| idv(&r[1]) == Some(user) && text(&r[2]).as_deref() == Some(from))
            {
                let Some(id) = idv(&r[0]) else { continue };
                if self.set_text(tx, entity, id, "state", to).await? {
                    n += 1;
                }
            }
            counts.push((action.to_owned(), n));
        }
        if self.invitations {
            let all = infra(rows(tx, INVITATION, &["id", "user", "state"]).await)?;
            let mut n = 0u64;
            for r in all
                .iter()
                .filter(|r| idv(&r[1]) == Some(user) && text(&r[2]).as_deref() == Some("pending"))
            {
                let Some(id) = idv(&r[0]) else { continue };
                if self
                    .set_text(tx, INVITATION, id, "state", "revoked")
                    .await?
                {
                    n += 1;
                }
            }
            counts.push(("cancel_invitations".to_owned(), n));
        }
        Ok(counts)
    }

    async fn set_text(
        &self,
        tx: &mut Tx,
        entity: &str,
        id: RecordId,
        field: &str,
        value: &str,
    ) -> IdResult<bool> {
        let loaded = tx
            .load(&infra(nm(entity))?, id, RowLock::Update(Wait::Block))
            .await
            .map_err(|e| store(&e))?;
        let Some(loaded) = loaded else {
            return Ok(false);
        };
        let mut data = loaded.data;
        put_fields(&mut data, vec![(field, value.into())])?;
        tx.update(&data, loaded.version)
            .await
            .map_err(|e| store(&e))?;
        Ok(true)
    }

    fn audit(&self, caller: RecordId, op: &str, target: Option<RecordId>, outcome: &str) {
        tracing::info!(
            target: AUDIT,
            site = %self.site,
            actor = %caller,
            op,
            target = target.map(|t| t.to_string()).as_deref().unwrap_or(""),
            outcome,
            "identity administration"
        );
    }

    /// The caller of the request (authenticated, not restricted).
    async fn caller(&self, ctx: &RequestCtx) -> IdResult<RecordId> {
        if ctx.site.as_str() != self.site {
            return Err(denied());
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if auth.is_restricted(&who) {
            return Err(denied());
        }
        Ok(who.user_id)
    }

    async fn issue_invitation(
        &self,
        tx: &mut Tx,
        user: RecordId,
        email: &str,
        now: OffsetDateTime,
    ) -> IdResult<(RecordId, String, OffsetDateTime)> {
        if !self.invitations {
            return Err(unavailable());
        }
        let token = spot_auth::OpaqueToken::generate();
        let digest: String = token
            .digest()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let expires = now + time::Duration::hours(i64::from(self.settings.invitation_ttl_hours));
        let defs = infra(spot_platform_app::definitions())?;
        let mut f = infra(blank(&defs, INVITATION))?;
        infra(put(
            &mut f,
            vec![
                ("user", Value::Id(user)),
                ("email", email.into()),
                ("state", "pending".into()),
                ("expires_at", dt(expires)),
                ("sent_at", dt(now)),
                ("digest", digest.as_str().into()),
            ],
        ))?;
        let id = RecordId::new_v7();
        tx.insert(&infra(doc(INVITATION, id, f))?)
            .await
            .map_err(|e| store(&e))?;
        Ok((id, token.as_str().to_owned(), expires))
    }

    async fn deliver(&self, email: &str, token: &str, expires: OffsetDateTime) {
        match &self.mail {
            Some(mail) => {
                if mail.send(email, token, expires).await.is_err() {
                    tracing::warn!(target: AUDIT, site = %self.site, "an invitation could not be queued for mail; it stays pending");
                }
            }
            None => {
                tracing::warn!(target: AUDIT, site = %self.site, "no mail module: an invitation was created but not sent")
            }
        }
    }
}

fn put_fields(data: &mut DocumentData, pairs: Vec<(&str, Value)>) -> IdResult<()> {
    for (k, v) in pairs {
        data.fields.insert(infra(nm(k))?, v);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// UserAdmin
// ---------------------------------------------------------------------------------------------

impl UserAdmin for IdentityUsers {
    fn user_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        q: UserListIn,
    ) -> BoxFuture<'a, IdResult<Page<UserOut>>> {
        Box::pin(async move {
            let _ = self.caller(ctx).await?;
            let mut tx = self.begin("identity_user_list", true).await?;
            let result = async {
                let users = self.user_rows(&mut tx).await?;
                let roles = self.roles_of(&mut tx).await?;
                let after = q
                    .page
                    .cursor
                    .as_deref()
                    .and_then(spot_http::api::codec::parse_record_id);
                let needle = q.q.as_deref().map(str::to_lowercase);
                let mut items: Vec<UserOut> = users
                    .iter()
                    .filter(|r| after.is_none_or(|a| r.id > a))
                    .filter(|r| q.status.as_deref().is_none_or(|s| r.state == s))
                    .filter(|r| {
                        needle.as_deref().is_none_or(|n| {
                            r.name.to_lowercase().starts_with(n)
                                || r.full_name.to_lowercase().starts_with(n)
                        })
                    })
                    .take(q.page.limit + 1)
                    .map(|r| Self::out(r, &roles, 1))
                    .collect();
                let next = (items.len() > q.page.limit).then(|| {
                    items.truncate(q.page.limit);
                    items.last().map(|u| u.id.clone()).unwrap_or_default()
                });
                Ok(Page {
                    items,
                    next_cursor: next,
                })
            }
            .await;
            self.finish(tx, result).await
        })
    }

    fn user_create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        input: UserCreateIn,
    ) -> BoxFuture<'a, IdResult<UserOut>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let now = self.clock.now_utc();
            let user_type = kind_of(&input.audience)?;
            let email = input.email.trim().to_lowercase();
            let id = RecordId::new_v7();
            let candidate = User::new(id, user_type, &email, input.full_name.trim(), now);
            candidate.validate().map_err(|e| iam(&e))?;
            if input.invite && !self.invitations {
                return Err(unavailable());
            }
            let mut tx = self.begin("identity_user_create", false).await?;
            let result = async {
                if infra(find_id(&mut tx, doctype::USER, "user_name", &email).await)?.is_some()
                    || infra(find_id(&mut tx, doctype::USER, "email", &email).await)?.is_some()
                {
                    return Err(ApiError::from_code(Code::IdentityUserExists));
                }
                let names: Vec<String> = input.roles.clone();
                crate::users::outside::check_new_assignments(&mut tx, user_type_text(user_type), &names)
                    .await
                    .map_err(|_| invalid("roles"))?;
                let defs = infra(spot_platform_app::definitions())?;
                let mut f = infra(blank(&defs, doctype::USER))?;
                infra(put(
                    &mut f,
                    vec![
                        ("user_name", email.as_str().into()),
                        ("email", email.as_str().into()),
                        ("full_name", input.full_name.trim().into()),
                        ("user_type", user_type_text(user_type).into()),
                        ("state", if input.invite { "invited" } else { "active" }.into()),
                        ("language", "en".into()),
                        ("time_zone", "UTC".into()),
                        ("failed_attempts", Value::Int(0)),
                    ],
                ))?;
                tx.insert(&infra(doc(doctype::USER, id, f))?)
                    .await
                    .map_err(|e| store(&e))?;
                let known = infra(rows(&mut tx, doctype::ROLE, &["id", "role_name"]).await)?;
                for name in &names {
                    let role = known
                        .iter()
                        .find(|r| text(&r[1]).as_deref() == Some(name.as_str()))
                        .and_then(|r| idv(&r[0]))
                        .ok_or_else(|| invalid("roles"))?;
                    let mut a = infra(blank(&defs, doctype::ROLE_ASSIGNMENT))?;
                    infra(put(
                        &mut a,
                        vec![
                            ("user", Value::Id(id)),
                            ("role", Value::Id(role)),
                            ("source", "manual".into()),
                            ("state", "active".into()),
                            ("valid_from", dt(now)),
                            ("reason", "Assigned when the user was created.".into()),
                        ],
                    ))?;
                    tx.insert(&infra(doc(doctype::ROLE_ASSIGNMENT, RecordId::new_v7(), a))?)
                        .await
                        .map_err(|e| store(&e))?;
                }
                let source = if input.invite { "invitation" } else { "admin" };
                self.event(
                    &mut tx,
                    "identity_user_created",
                    id,
                    by,
                    json!({"user": id.to_string(), "user_type": user_type_text(user_type), "source": source}),
                )
                .await?;
                let sent = if input.invite {
                    Some(self.issue_invitation(&mut tx, id, &email, now).await?)
                } else {
                    None
                };
                Ok(sent)
            }
            .await;
            let sent = self.finish(tx, result).await?;
            self.audit(by, "user_create", Some(id), "ok");
            if let Some((_, token, expires)) = sent {
                self.deliver(&email, &token, expires).await;
            }
            let mut tx = self.begin("identity_user_create_read", true).await?;
            let read = async {
                let (row, version) = self.view_of(&mut tx, id).await?;
                let roles = self.roles_of(&mut tx).await?;
                Ok(Self::out(&row, &roles, version))
            }
            .await;
            self.finish(tx, read).await
        })
    }

    fn user_get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<UserOut>> {
        Box::pin(async move {
            let _ = self.caller(ctx).await?;
            let id = record(id)?;
            let mut tx = self.begin("identity_user_get", true).await?;
            let result = async {
                let (row, version) = self.view_of(&mut tx, id).await?;
                let roles = self.roles_of(&mut tx).await?;
                Ok(Self::out(&row, &roles, version))
            }
            .await;
            self.finish(tx, result).await
        })
    }

    fn user_patch<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        version: u64,
        patch: UserPatchIn,
    ) -> BoxFuture<'a, IdResult<UserOut>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let id = record(id)?;
            let mut tx = self.begin("identity_user_patch", false).await?;
            let result = async {
                let loaded = self.lock_user(&mut tx, id).await?;
                if loaded.version.0 != version {
                    return Err(ApiError::from_code(Code::DocumentStaleVersion));
                }
                let mut data = loaded.data;
                if let Some(name) = &patch.full_name {
                    let name = name.trim();
                    let probe = User::new(
                        id,
                        UserType::Internal,
                        "probe.name",
                        name,
                        OffsetDateTime::UNIX_EPOCH,
                    );
                    probe.validate().map_err(|e| iam(&e))?;
                    put_fields(&mut data, vec![("full_name", name.into())])?;
                }
                if let Some(audience) = &patch.audience {
                    let new_type = kind_of(audience)?;
                    // Moving between audiences changes which roles may be held: the portal rule
                    // is checked against the user's current role names.
                    let roles = self.roles_of(&mut tx).await?;
                    let names: Vec<String> = roles
                        .iter()
                        .filter(|(u, _)| *u == id)
                        .map(|(_, n)| n.clone())
                        .collect();
                    crate::users::outside::check_new_assignments(
                        &mut tx,
                        user_type_text(new_type),
                        &names,
                    )
                    .await
                    .map_err(|_| invalid("audience"))?;
                    put_fields(
                        &mut data,
                        vec![("user_type", user_type_text(new_type).into())],
                    )?;
                }
                let new_version = tx
                    .update(&data, loaded.version)
                    .await
                    .map_err(|e| store(&e))?;
                let (row, _) = self.view_of(&mut tx, id).await?;
                let roles = self.roles_of(&mut tx).await?;
                Ok(Self::out(&row, &roles, new_version.0))
            }
            .await;
            let out = self.finish(tx, result).await?;
            self.audit(by, "user_patch", Some(id), "ok");
            Ok(out)
        })
    }

    fn user_disable<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<UserChangeOut>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let id = record(id)?;
            let now = self.clock.now_utc();
            let mut tx = self.begin("identity_user_disable", false).await?;
            let result = async {
                // Lock the administrators (and with them the target when it is one), then check.
                let admins = self.admins(&mut tx).await?;
                let (row, _) = self.view_of(&mut tx, id).await?;
                if matches!(row.state.as_str(), "disabled" | "anonymised") {
                    let roles = self.roles_of(&mut tx).await?;
                    return Ok(UserChangeOut {
                        user: Self::out(&row, &roles, 1),
                        leaver_actions: Vec::new(),
                    });
                }
                check_admin_invariant(
                    &admins,
                    &Change {
                        user: id,
                        kind: ChangeKind::Disable,
                    },
                )
                .map_err(|e| iam(&e))?;
                let user = User::new(
                    id,
                    user_type_of(&row.user_type),
                    &row.name,
                    &row.full_name,
                    OffsetDateTime::UNIX_EPOCH,
                );
                let actor = Actor::User { id: by, via: None };
                let step = user
                    .disable(&actor, "Disabled by an administrator.", now)
                    .map_err(|e| iam(&e))?;
                let version = self
                    .set_state(&mut tx, id, "disabled", step.new_state.deactivated_at)
                    .await?;
                let mut actions = Vec::new();
                if step.actions.contains(&LeaverAction::EndSessions) {
                    actions = self.leave(&mut tx, id).await?;
                }
                let names: Vec<Json> = actions
                    .iter()
                    .map(|(a, _)| Json::from(a.as_str()))
                    .collect();
                self.event(
                    &mut tx,
                    "identity_user_disabled",
                    id,
                    by,
                    json!({"user": id.to_string(), "source": "admin", "actions": names}),
                )
                .await?;
                let (row, _) = self.view_of(&mut tx, id).await?;
                let roles = self.roles_of(&mut tx).await?;
                Ok(UserChangeOut {
                    user: Self::out(&row, &roles, version),
                    leaver_actions: actions,
                })
            }
            .await;
            let out = self.finish(tx, result).await;
            self.audit(
                by,
                "user_disable",
                Some(id),
                if out.is_ok() { "ok" } else { "refused" },
            );
            out
        })
    }

    fn user_enable<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<UserChangeOut>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let id = record(id)?;
            let now = self.clock.now_utc();
            let mut tx = self.begin("identity_user_enable", false).await?;
            let result = async {
                let (row, _) = self.view_of(&mut tx, id).await?;
                if row.state == "anonymised" {
                    return Err(invalid("state"));
                }
                if row.state != "disabled" {
                    let roles = self.roles_of(&mut tx).await?;
                    return Ok(UserChangeOut {
                        user: Self::out(&row, &roles, 1),
                        leaver_actions: Vec::new(),
                    });
                }
                crate::users::outside::check_user_assignments(&mut tx, id)
                    .await
                    .map_err(|_| invalid("roles"))?;
                let _ = now;
                let version = self.set_state(&mut tx, id, "active", None).await?;
                self.event(
                    &mut tx,
                    "identity_user_enabled",
                    id,
                    by,
                    json!({"user": id.to_string()}),
                )
                .await?;
                let (row, _) = self.view_of(&mut tx, id).await?;
                let roles = self.roles_of(&mut tx).await?;
                Ok(UserChangeOut {
                    user: Self::out(&row, &roles, version),
                    leaver_actions: Vec::new(),
                })
            }
            .await;
            let out = self.finish(tx, result).await;
            self.audit(
                by,
                "user_enable",
                Some(id),
                if out.is_ok() { "ok" } else { "refused" },
            );
            out
        })
    }

    fn user_anonymise<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        confirm_email: String,
    ) -> BoxFuture<'a, IdResult<UserOut>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let id = record(id)?;
            let mut tx = self.begin("identity_user_anonymise", false).await?;
            let result = async {
                let admins = self.admins(&mut tx).await?;
                let (row, _) = self.view_of(&mut tx, id).await?;
                let shown = row.email.clone().unwrap_or_else(|| row.name.clone());
                if row.state != "anonymised" && !shown.eq_ignore_ascii_case(confirm_email.trim()) {
                    return Err(invalid("confirm_email"));
                }
                check_admin_invariant(
                    &admins,
                    &Change {
                        user: id,
                        kind: ChangeKind::Delete,
                    },
                )
                .map_err(|e| iam(&e))?;
                let user = User::new(
                    id,
                    user_type_of(&row.user_type),
                    &row.name,
                    &row.full_name,
                    OffsetDateTime::UNIX_EPOCH,
                );
                let anon = spot_iam_directory::anonymise(&user);
                let loaded = self.lock_user(&mut tx, id).await?;
                let mut data = loaded.data;
                put_fields(
                    &mut data,
                    vec![
                        ("user_name", anon.user().sign_in_name.as_str().into()),
                        ("email", Value::Null),
                        ("full_name", anon.user().display_name.as_str().into()),
                        ("state", "anonymised".into()),
                        ("manager", Value::Null),
                        ("party_type", Value::Null),
                        ("party", Value::Null),
                        ("scim_profile", Value::Null),
                        ("scim_external_id", Value::Null),
                    ],
                )?;
                let version = tx
                    .update(&data, loaded.version)
                    .await
                    .map_err(|e| store(&e))?;
                let _ = self.leave(&mut tx, id).await?;
                self.event(
                    &mut tx,
                    "identity_user_anonymised",
                    id,
                    by,
                    json!({"user": id.to_string()}),
                )
                .await?;
                let (row, _) = self.view_of(&mut tx, id).await?;
                Ok(Self::out(&row, &[], version.0))
            }
            .await;
            let out = self.finish(tx, result).await;
            self.audit(
                by,
                "user_anonymise",
                Some(id),
                if out.is_ok() { "ok" } else { "refused" },
            );
            out
        })
    }

    fn user_recover<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        kind: String,
    ) -> BoxFuture<'a, IdResult<RecoverOut>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let id = record(id)?;
            let kind_value = match kind.as_str() {
                "password_reset" => RecoveryKind::Password,
                "second_factor_reset" => RecoveryKind::SecondFactor,
                _ => return Err(invalid("kind")),
            };
            let mut tx = self.begin("identity_user_recover", false).await?;
            let result = async {
                let admins = self.admins(&mut tx).await?;
                let (row, _) = self.view_of(&mut tx, id).await?;
                let target = Target {
                    user_type: user_type_of(&row.user_type),
                    enabled: row.state == "active",
                    is_administrator: admins.is_enabled_tenant_admin(id),
                };
                let steps = required_steps(
                    &RecoveryPolicy {
                        self_service_password: false,
                        self_service_second_factor: false,
                    },
                    &RecoveryRequest { target: id, initiated_by: by, kind: kind_value },
                    &Requester { is_administrator: admins.is_enabled_tenant_admin(by) },
                    &target,
                )
                .map_err(|e| iam(&e))?;
                let names: Vec<Json> = steps
                    .iter()
                    .map(|s| Json::from(format!("{s:?}")))
                    .map(|s| Json::from(snake(s.as_str().unwrap_or_default())))
                    .collect();
                let second = steps.iter().any(|s| format!("{s:?}") == "SecondApprover");
                self.event(
                    &mut tx,
                    "identity_recovery_requested",
                    id,
                    by,
                    json!({"user": id.to_string(), "kind": if kind == "password_reset" {"password"} else {"second_factor"},
                           "initiated_by": by.to_string(), "steps": names}),
                )
                .await?;
                Ok(if second { RecoverOut::NeedsApproval } else { RecoverOut::Sent })
            }
            .await;
            let out = self.finish(tx, result).await;
            self.audit(
                by,
                "user_recover",
                Some(id),
                if out.is_ok() { "ok" } else { "refused" },
            );
            out
        })
    }

    fn invitation_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: PageIn,
    ) -> BoxFuture<'a, IdResult<Page<InvitationOut>>> {
        Box::pin(async move {
            let _ = self.caller(ctx).await?;
            if !self.invitations {
                return Err(unavailable());
            }
            let mut tx = self.begin("identity_invitation_list", true).await?;
            let result = async {
                let all = infra(
                    rows(
                        &mut tx,
                        INVITATION,
                        &["id", "user", "email", "state", "expires_at", "sent_at"],
                    )
                    .await,
                )?;
                let now = self.clock.now_utc();
                let after = page
                    .cursor
                    .as_deref()
                    .and_then(spot_http::api::codec::parse_record_id);
                let mut items: Vec<(RecordId, InvitationOut)> = all
                    .iter()
                    .filter_map(|r| {
                        let id = idv(&r[0])?;
                        let expires = at(&r[4])?;
                        let mut status = text(&r[3])?;
                        if status == "pending" && expires < now {
                            status = "expired".to_owned();
                        }
                        Some((
                            id,
                            InvitationOut {
                                id: id.to_string(),
                                user_id: idv(&r[1])?.to_string(),
                                email: text(&r[2])?,
                                status,
                                expires_at: rfc(expires),
                                sent_at: rfc(at(&r[5])?),
                            },
                        ))
                    })
                    .filter(|(id, _)| after.is_none_or(|a| *id > a))
                    .collect();
                items.sort_by_key(|(id, _)| *id);
                let mut out: Vec<InvitationOut> = items.into_iter().map(|(_, i)| i).collect();
                let next = (out.len() > page.limit).then(|| {
                    out.truncate(page.limit);
                    out.last().map(|i| i.id.clone()).unwrap_or_default()
                });
                Ok(Page {
                    items: out,
                    next_cursor: next,
                })
            }
            .await;
            self.finish(tx, result).await
        })
    }

    fn invitation_revoke<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<()>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            if !self.invitations {
                return Err(unavailable());
            }
            let id = record(id)?;
            let mut tx = self.begin("identity_invitation_revoke", false).await?;
            let result = async {
                let loaded = tx
                    .load(&infra(nm(INVITATION))?, id, RowLock::Update(Wait::Block))
                    .await
                    .map_err(|e| store(&e))?
                    .ok_or_else(not_found)?;
                let state = nm("state")
                    .ok()
                    .and_then(|n| loaded.data.fields.get(&n).and_then(text));
                if state.as_deref() == Some("pending") {
                    let mut data = loaded.data;
                    put_fields(&mut data, vec![("state", "revoked".into())])?;
                    tx.update(&data, loaded.version)
                        .await
                        .map_err(|e| store(&e))?;
                }
                Ok(())
            }
            .await;
            let out = self.finish(tx, result).await;
            self.audit(
                by,
                "invitation_revoke",
                Some(id),
                if out.is_ok() { "ok" } else { "refused" },
            );
            out
        })
    }

    fn invitation_resend<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<()>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            if !self.invitations {
                return Err(unavailable());
            }
            let id = record(id)?;
            let now = self.clock.now_utc();
            let mut tx = self.begin("identity_invitation_resend", false).await?;
            let result = async {
                let loaded = tx
                    .load(&infra(nm(INVITATION))?, id, RowLock::Update(Wait::Block))
                    .await
                    .map_err(|e| store(&e))?
                    .ok_or_else(not_found)?;
                let field = |n: &str| nm(n).ok().and_then(|n| loaded.data.fields.get(&n).cloned());
                let user = field("user")
                    .as_ref()
                    .and_then(idv)
                    .ok_or_else(unavailable)?;
                let email = field("email")
                    .as_ref()
                    .and_then(text)
                    .ok_or_else(unavailable)?;
                let (row, _) = self.view_of(&mut tx, user).await?;
                if row.state != "invited" {
                    return Err(invalid("state"));
                }
                // The old invitation ends; a new token replaces it.
                let mut data = loaded.data;
                put_fields(&mut data, vec![("state", "revoked".into())])?;
                tx.update(&data, loaded.version)
                    .await
                    .map_err(|e| store(&e))?;
                self.issue_invitation(&mut tx, user, &email, now)
                    .await
                    .map(|(_, token, expires)| (email, token, expires))
            }
            .await;
            let (email, token, expires) = self.finish(tx, result).await?;
            self.audit(by, "invitation_resend", Some(id), "ok");
            self.deliver(&email, &token, expires).await;
            Ok(())
        })
    }
}

fn user_type_text(t: UserType) -> &'static str {
    match t {
        UserType::Internal => "internal",
        UserType::Portal => "portal",
        UserType::Service => "service",
        UserType::Agent => "agent",
    }
}

fn user_type_of(text: &str) -> UserType {
    match text {
        "portal" => UserType::Portal,
        "service" => UserType::Service,
        "agent" => UserType::Agent,
        _ => UserType::Internal,
    }
}

/// `SecondApprover` as `second_approver`.
fn snake(camel: &str) -> String {
    let mut out = String::new();
    for (i, c) in camel.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The ports the group needs: permission, step-up, audit, and the composed admin port
// ---------------------------------------------------------------------------------------------

/// Role names that carry the `identity.*` permissions, by permission.
fn holds(roles: &[String], permission: IdentityPermission) -> bool {
    let has = |n: &str| roles.iter().any(|r| r == n);
    match permission {
        IdentityPermission::Impersonate | IdentityPermission::ImpersonationOwner => false,
        IdentityPermission::Read | IdentityPermission::Manage => {
            has("Tenant Administrator") || has("User Manager")
        }
        _ => has("Tenant Administrator"),
    }
}

impl IdentityPermissionPort for IdentityUsers {
    fn check<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        permission: IdentityPermission,
    ) -> BoxFuture<'a, IdResult<IdentityCaller>> {
        Box::pin(async move {
            let user = self.caller(ctx).await?;
            let mut tx = self.begin("identity_permission", true).await?;
            let result = async {
                let roles: Vec<String> = self
                    .roles_of(&mut tx)
                    .await?
                    .into_iter()
                    .filter(|(u, _)| *u == user)
                    .map(|(_, n)| n)
                    .collect();
                let (row, _) = self.view_of(&mut tx, user).await?;
                let audience = match row.user_type.as_str() {
                    "internal" => CallerAudience::Staff,
                    "portal" => CallerAudience::Partner,
                    _ => CallerAudience::Consumer,
                };
                if row.state != "active" || !holds(&roles, permission) {
                    return Err(denied());
                }
                Ok(IdentityCaller {
                    user_id: user.to_string(),
                    audience,
                })
            }
            .await;
            self.finish(tx, result).await
        })
    }
}

impl IdentityStepUpPort for IdentityUsers {
    fn is_recent<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<bool>> {
        Box::pin(async move {
            if ctx.site.as_str() != self.site {
                return Ok(false);
            }
            let auth = self.live.auth().ok_or_else(unavailable)?;
            let who = auth.authenticate_on(ctx, PathClass::Api).await?;
            let Some(session) = who
                .session_id
                .as_deref()
                .and_then(spot_http::api::codec::parse_record_id)
            else {
                return Ok(false);
            };
            let mut tx = self.begin("identity_step_up", true).await?;
            let loaded = tx
                .load(&infra(nm(doctype::SESSION))?, session, RowLock::None)
                .await;
            let _ = tx.rollback().await;
            let created = loaded.map_err(|e| store(&e))?.and_then(|d| {
                nm("created_at")
                    .ok()
                    .and_then(|n| d.data.fields.get(&n).and_then(at))
            });
            let now = self.clock.now_utc();
            Ok(created
                .is_some_and(|c| (now - c).whole_seconds() <= self.settings.step_up_window_secs))
        })
    }
}

impl IdentityAuditPort for IdentityUsers {
    fn record<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        entry: AuditEntry,
    ) -> BoxFuture<'a, IdResult<()>> {
        Box::pin(async move {
            tracing::info!(
                target: AUDIT,
                site = %self.site,
                op = entry.operation,
                outcome = ?entry.outcome,
                code = %entry.code,
                actor = entry.actor.as_deref().unwrap_or(""),
                target_id = entry.target.as_deref().unwrap_or(""),
                "identity administration refused"
            );
            Ok(())
        })
    }
}

/// The composed port: users from this module, the other areas from whichever module provided them.
struct IdentityAdmin {
    users: Arc<IdentityUsers>,
    roles: Option<Arc<dyn RoleAdmin>>,
    assignments: Option<Arc<dyn AssignmentAdmin>>,
    keys: Option<Arc<dyn KeyAdmin>>,
    sessions: Option<Arc<dyn SessionAdmin>>,
}

impl IdentityAdminPort for IdentityAdmin {
    fn users(&self) -> Option<&dyn UserAdmin> {
        Some(&*self.users)
    }
    fn roles(&self) -> Option<&dyn RoleAdmin> {
        self.roles.as_deref()
    }
    fn assignments(&self) -> Option<&dyn AssignmentAdmin> {
        self.assignments.as_deref()
    }
    fn keys(&self) -> Option<&dyn KeyAdmin> {
        self.keys.as_deref()
    }
    fn sessions(&self) -> Option<&dyn SessionAdmin> {
        self.sessions.as_deref()
    }
}

// ---------------------------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------------------------

/// Joins the site unless `[services.identity_admin] enabled = false`.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("identity_admin")?;
    if !settings.enabled {
        return Ok(());
    }
    if !(1..=720).contains(&settings.invitation_ttl_hours)
        || !(30..=86_400).contains(&settings.step_up_window_secs)
    {
        return Err(Error(
            "services.identity_admin: invitation_ttl_hours is 1 to 720 and step_up_window_secs 30 to 86400"
                .to_owned(),
        ));
    }
    let (site, live, catalog) = {
        let env = ctx.env()?;
        (
            env.site.site.clone(),
            Arc::clone(env.live),
            env.permission.catalog.clone(),
        )
    };
    let invitations = spot_platform_app::definitions()
        .map(|defs| defs.iter().any(|d| d.name == INVITATION))
        .unwrap_or(false);
    let mail = ctx.port::<dyn InvitationMail>();
    let service = Arc::new(IdentityUsers {
        site,
        live,
        clock: Arc::clone(ctx.clock()),
        catalog,
        settings,
        mail: mail.clone(),
        invitations,
    });
    if ctx.assembling() {
        let (has_mail, has_doc) = (mail.is_some(), invitations);
        ctx.add(Idle::new("identity_admin", Level::Optional, move || {
            Box::pin(async move {
                if has_doc && has_mail {
                    Ok(())
                } else {
                    Err("invitations are unavailable (no `invitation` DocType or no mail module): users can still be created without an invitation".to_owned())
                }
            })
        }));
    }
    let users: Arc<dyn UserAdmin> = service.clone();
    ctx.provide(users)?;
    ctx.finally(move |ctx| {
        let admin: Arc<dyn IdentityAdminPort> = Arc::new(IdentityAdmin {
            users: service.clone(),
            roles: ctx.port::<dyn RoleAdmin>(),
            assignments: ctx.port::<dyn AssignmentAdmin>(),
            keys: ctx.port::<dyn KeyAdmin>(),
            sessions: ctx.port::<dyn SessionAdmin>(),
        });
        let permission: Arc<dyn IdentityPermissionPort> = service.clone();
        let step_up: Arc<dyn IdentityStepUpPort> = service.clone();
        let audit: Arc<dyn IdentityAuditPort> = service;
        let mut ports = Ports::new();
        ports.provide::<dyn IdentityAdminPort>(admin);
        ports.provide::<dyn IdentityPermissionPort>(permission);
        ports.provide::<dyn IdentityStepUpPort>(step_up);
        ports.provide::<dyn IdentityAuditPort>(audit);
        let mounted = routes::mount(&[identity_registration()], &ports, &[]);
        let router = mounted.authenticated;
        if let Some((module, reason)) = mounted.report.skipped.first() {
            tracing::error!(
                module = *module,
                ?reason,
                "the identity administration routes were not mounted"
            );
        }
        ctx.router_layer(move |base| base.merge(router));
        Ok(())
    });
    Ok(())
}

/// The route group of the identity administration, registered by hand: `spot-http` mounts only
/// the groups it is given ports for, and the host gives the router its ports one module at a time.
fn identity_registration() -> Registration {
    Registration {
        name: "identity",
        serves: routes::identity::SERVES,
        required_ports: routes::identity::required_ports,
        routes: routes::identity::routes,
        public: None,
    }
}
