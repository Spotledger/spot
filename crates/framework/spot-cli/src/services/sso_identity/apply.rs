//! Applying a sign-in resolution to the tenant's records (WP-IAM-05.T16), in the caller's
//! transaction: nothing is durable until the caller commits, and a refusal leaves nothing behind.
//!
//! | Resolution | What happens |
//! | --- | --- |
//! | `SignIn` | the user's `last_sign_in` moves; nothing else |
//! | `LinkViaInvitation` | the invitation is locked and **re-checked** (pending, not expired by the injected clock, addressed to this e-mail, the provider verified that e-mail); the identity is linked to the invited user; the invitation becomes `accepted`; an `invited` user becomes `active` |
//! | `CreateUser` | a user is created by a rule of the tenant's policy, `active`, **with no role at all**, and the identity is linked to it |
//! | `ConsumerSignIn` | refused: there is no consumer principal store yet (WP-CON-03), and a consumer is never a user |
//! | `RequireLocalConfirmation`, `Denied` | refused (the host refuses them before calling; this is defence in depth) |
//!
//! **Linking is an account takeover path.** The only ways an identity reaches a user are an
//! already linked `(issuer, subject)` and a verified, unexpired invitation; an e-mail match alone
//! never links. The invitation is read again under a row lock here even though the snapshot read
//! it: two sign-ins with one invitation serialise on the lock and the second finds it accepted.
//! The `(provider, subject)` pair is unique in storage, so two concurrent links of one identity to
//! two users cannot both win.

use spot_events_contract::payload::{EventMeta, PayloadDocument};
use spot_events_contract::{Actor, Topic};
use spot_meta::Definition;
use spot_platform_app::doctype;
use spot_sso::{Resolution, SignInPolicy, SsoIdentity, codes};
use spot_storage_contract::{ErrorKind, NodeRef, RecordId, Transaction, Value};
use time::OffsetDateTime;

use super::directory::{PENDING, lock, user_type_word};
use super::{eq_ids, field};
use crate::services::scim_store::{at, blank, doc, dt, idv, nm, put, text};
use crate::services::sso::IdentityFailure;

const INVITATION: &str = "invitation";

/// What applying needs besides the transaction.
pub struct ApplyEnv<'a> {
    /// The injected current time.
    pub now: OffsetDateTime,
    /// The platform DocType definitions (to build full records).
    pub defs: &'a [Definition],
    /// The tenant's sign-in policy (the rule that allowed a just-in-time user is judged again).
    pub policy: &'a SignInPolicy,
}

fn refused(code: &'static str) -> IdentityFailure {
    IdentityFailure::Refused(code)
}

fn unavailable<E>(_: E) -> IdentityFailure {
    IdentityFailure::Unavailable
}

/// A write failure: a duplicate key is a conflict the caller refuses, anything else is an outage.
fn written<T>(
    result: Result<T, spot_storage_contract::StorageError>,
) -> Result<T, IdentityFailure> {
    result.map_err(|e| match e.kind {
        ErrorKind::UniqueViolation | ErrorKind::AlreadyExists => {
            IdentityFailure::Refused(codes::IDENTITY_OTHER_USER)
        }
        _ => IdentityFailure::Unavailable,
    })
}

pub(super) async fn event<T: Transaction>(
    tx: &mut T,
    now: OffsetDateTime,
    topic: &str,
    user: RecordId,
    data: serde_json::Value,
) -> Result<(), IdentityFailure> {
    let id = RecordId::new_v7();
    let meta = EventMeta {
        occurred_at: now,
        actor: Actor::User {
            id: user,
            via: None,
        },
        command_id: None,
        causation_id: None,
        correlation_id: id,
        trace_id: None,
    };
    let topic = Topic::any(topic).map_err(unavailable)?;
    let subject = NodeRef {
        entity: nm(doctype::USER).map_err(unavailable)?,
        id: user,
    };
    let event = PayloadDocument { meta, data }
        .to_outbox_event(id, &topic, subject, None, 1)
        .map_err(unavailable)?;
    tx.enqueue(&event).await.map_err(unavailable)
}

async fn link<T: Transaction>(
    tx: &mut T,
    env: &ApplyEnv<'_>,
    identity: &SsoIdentity,
    user: RecordId,
) -> Result<(), IdentityFailure> {
    // The pair is unique: an existing link to another user refuses, to this user is idempotent.
    let existing = eq_ids(
        tx,
        doctype::LINKED_IDENTITY,
        &[
            ("provider", &identity.issuer),
            ("subject", &identity.subject),
        ],
    )
    .await
    .map_err(unavailable)?;
    if let Some(id) = existing.into_iter().next() {
        let entity = nm(doctype::LINKED_IDENTITY).map_err(unavailable)?;
        let loaded = tx
            .load(&entity, id, spot_storage_contract::RowLock::None)
            .await
            .map_err(unavailable)?;
        if let Some(loaded) = loaded
            && idv(&field(&loaded.data, "user")) != Some(user)
        {
            return Err(refused(codes::IDENTITY_OTHER_USER));
        }
        return Ok(());
    }
    let mut fields = blank(env.defs, doctype::LINKED_IDENTITY).map_err(unavailable)?;
    put(
        &mut fields,
        vec![
            ("user", Value::Id(user)),
            ("provider", identity.issuer.as_str().into()),
            ("subject", identity.subject.as_str().into()),
            ("last_seen", dt(env.now)),
        ],
    )
    .map_err(unavailable)?;
    let record = doc(doctype::LINKED_IDENTITY, RecordId::new_v7(), fields).map_err(unavailable)?;
    written(tx.insert(&record).await).map(|_| ())
}

/// Sets `last_sign_in` on the user and, when `activate`, moves an `invited` user to `active`.
async fn touch_user<T: Transaction>(
    tx: &mut T,
    env: &ApplyEnv<'_>,
    user: RecordId,
    activate: bool,
) -> Result<(), IdentityFailure> {
    let Some(loaded) = lock(tx, doctype::USER, user).await.map_err(unavailable)? else {
        return Err(refused(codes::UNKNOWN_IDENTITY));
    };
    let state = text(&field(&loaded.data, "state")).unwrap_or_default();
    let activating = activate && state == "invited";
    if state != "active" && !activating {
        // Disabled, anonymised or still invited: not signed in by this path.
        return Err(refused(codes::USER_DISABLED));
    }
    let mut data = loaded.data;
    data.fields
        .insert(nm("last_sign_in").map_err(unavailable)?, dt(env.now));
    if activating {
        data.fields
            .insert(nm("state").map_err(unavailable)?, "active".into());
    }
    written(tx.update(&data, loaded.version).await)?;
    if activating {
        event(
            tx,
            env.now,
            "identity_user_enabled",
            user,
            serde_json::json!({"user": user.to_string(), "reason": "sso_invitation"}),
        )
        .await?;
    }
    Ok(())
}

/// Applies `resolution` to the records and returns the user to sign in.
///
/// # Errors
///
/// [`IdentityFailure::Refused`] with the stable `sso.*` code, or
/// [`IdentityFailure::Unavailable`] when a read or write fails. The caller rolls back on either.
pub async fn apply_in<T: Transaction>(
    tx: &mut T,
    env: &ApplyEnv<'_>,
    identity: &SsoIdentity,
    resolution: &Resolution,
) -> Result<RecordId, IdentityFailure> {
    match resolution {
        Resolution::SignIn { user, .. } => {
            touch_user(tx, env, *user, false).await?;
            Ok(*user)
        }
        Resolution::LinkViaInvitation { invitation, email } => {
            // The provider must have verified the address the invitation was sent to.
            let identity_email = identity.email.as_deref().map(str::to_ascii_lowercase);
            if !identity.email_verified
                || identity_email.as_deref() != Some(email.to_ascii_lowercase().as_str())
            {
                return Err(refused(codes::LINK_REQUIRES_PROOF));
            }
            let Some(loaded) = lock(tx, INVITATION, *invitation)
                .await
                .map_err(unavailable)?
            else {
                return Err(refused(codes::LINK_REQUIRES_PROOF));
            };
            let open = text(&field(&loaded.data, "state")).as_deref() == Some(PENDING)
                && at(&field(&loaded.data, "expires_at")).is_some_and(|t| env.now < t)
                && text(&field(&loaded.data, "email"))
                    .is_some_and(|e| e.eq_ignore_ascii_case(email));
            let Some(user) = idv(&field(&loaded.data, "user")).filter(|_| open) else {
                return Err(refused(codes::LINK_REQUIRES_PROOF));
            };
            link(tx, env, identity, user).await?;
            let mut data = loaded.data;
            data.fields
                .insert(nm("state").map_err(unavailable)?, "accepted".into());
            data.fields
                .insert(nm("accepted_at").map_err(unavailable)?, dt(env.now));
            written(tx.update(&data, loaded.version).await)?;
            touch_user(tx, env, user, true).await?;
            Ok(user)
        }
        Resolution::CreateUser {
            user_type,
            identity: new,
            ..
        } => {
            // The rule that allowed this is judged again; a domain rule needs a verified address.
            let rule = env.policy.jit.matching_rule(new);
            let allowed = rule.is_some_and(|r| {
                r.user_type == *user_type && (r.email_domain.is_none() || new.email_verified)
            });
            if !allowed {
                return Err(refused(codes::JIT_NOT_ALLOWED));
            }
            let Some(email) = new
                .email
                .as_deref()
                .map(|e| e.trim().to_ascii_lowercase())
                .filter(|e| !e.is_empty() && e.len() <= 254 && e.contains('@'))
            else {
                return Err(refused(codes::CLAIM_MISSING));
            };
            // A person who already has this address is never given a second account.
            let clash = eq_ids(tx, doctype::USER, &[("user_name", &email)])
                .await
                .map_err(unavailable)?;
            let clash_mail = eq_ids(tx, doctype::USER, &[("email", &email)])
                .await
                .map_err(unavailable)?;
            if !clash.is_empty() || !clash_mail.is_empty() {
                return Err(refused(codes::LINK_REQUIRES_PROOF));
            }
            let name = new
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|n| {
                    !n.is_empty() && n.chars().count() <= 200 && !n.chars().any(char::is_control)
                })
                .map_or_else(
                    || email.split('@').next().unwrap_or("user").to_owned(),
                    str::to_owned,
                );
            let id = RecordId::new_v7();
            let mut fields = blank(env.defs, doctype::USER).map_err(unavailable)?;
            put(
                &mut fields,
                vec![
                    ("user_name", email.as_str().into()),
                    ("email", email.as_str().into()),
                    ("full_name", name.as_str().into()),
                    ("user_type", user_type_word(*user_type).into()),
                    ("state", "active".into()),
                    ("language", "en".into()),
                    ("time_zone", "UTC".into()),
                    ("failed_attempts", Value::Int(0)),
                    ("last_sign_in", dt(env.now)),
                ],
            )
            .map_err(unavailable)?;
            let record = doc(doctype::USER, id, fields).map_err(unavailable)?;
            written(tx.insert(&record).await)?;
            link(tx, env, new, id).await?;
            event(
                tx,
                env.now,
                "identity_user_created",
                id,
                serde_json::json!({
                    "user": id.to_string(),
                    "user_type": user_type_word(*user_type),
                    "source": "sso_jit",
                }),
            )
            .await?;
            Ok(id)
        }
        Resolution::ConsumerSignIn { .. } => Err(refused(codes::SSO_NOT_ALLOWED)),
        Resolution::RequireLocalConfirmation { .. } => Err(refused(codes::LINK_REQUIRES_PROOF)),
        Resolution::Denied(error) => Err(refused(error.code())),
    }
}
