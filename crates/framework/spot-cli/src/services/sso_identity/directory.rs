//! The directory snapshot the sign-in resolution reads (WP-IAM-05.T16): what the tenant's own
//! records say about one provider identity, loaded in one read-only pass for one tenant.
//!
//! [`snapshot_in`] is generic over the storage transaction, so the real function runs over the
//! in-memory backend in the tests. It reads only the records the identity can reach (the users it
//! is linked to, the users whose sign-in name or address equals its e-mail, the invitations
//! addressed to that e-mail), never the whole directory, and it never reads another tenant: the
//! transaction is the site's own.
//!
//! What it reports is facts, not decisions: `spot_sso::resolve` decides. In particular an
//! **e-mail match is reported as a match and nothing more**: it never becomes a link (the
//! resolution answers `RequireLocalConfirmation` and the host refuses it).

use std::collections::BTreeMap;

use spot_core::Audience;
use spot_iam_directory::UserType;
use spot_platform_app::{doctype, role};
use spot_sso::{DirectoryUser, SecondFactor, SignInPolicy, SsoIdentity};
use spot_storage_contract::{RecordId, RowLock, Transaction, Value, Wait};
use time::OffsetDateTime;

use super::{Unavail, eq_ids, field, st};
use crate::services::scim_store::{at, idv, nm, rows, text};
use crate::services::sso::DirectorySnapshot;

/// The state of an invitation that can still be accepted.
pub const PENDING: &str = "pending";

/// What the snapshot needs besides the transaction.
pub struct SnapshotEnv<'a> {
    /// The injected current time.
    pub now: OffsetDateTime,
    /// The tenant's sign-in policy (read before; a policy that cannot be read refuses earlier).
    pub policy: &'a SignInPolicy,
    /// The configured providers: id to (healthy, audience).
    pub providers: &'a BTreeMap<String, (bool, Audience)>,
}

/// A user type from its stored word.
#[must_use]
pub fn user_type_of(word: &str) -> Option<UserType> {
    match word {
        "internal" => Some(UserType::Internal),
        "portal" => Some(UserType::Portal),
        "service" => Some(UserType::Service),
        "agent" => Some(UserType::Agent),
        _ => None,
    }
}

/// The stored word of a user type.
#[must_use]
pub fn user_type_word(kind: UserType) -> &'static str {
    match kind {
        UserType::Internal => "internal",
        UserType::Portal => "portal",
        UserType::Service => "service",
        UserType::Agent => "agent",
    }
}

/// The enabled tenant administrators: users with an active assignment of the Tenant
/// Administrator role whose own state is `active`.
///
/// # Errors
///
/// [`Unavail`] when a read fails.
pub async fn enabled_admins<T: Transaction>(tx: &mut T) -> Result<Vec<RecordId>, Unavail> {
    let roles = st(rows(tx, doctype::ROLE, &["id", "role_name"]).await)?;
    let Some(admin_role) = roles
        .iter()
        .find(|r| text(&r[1]).as_deref() == Some(role::TENANT_ADMINISTRATOR))
        .and_then(|r| idv(&r[0]))
    else {
        return Ok(Vec::new());
    };
    let assigned = st(rows(tx, doctype::ROLE_ASSIGNMENT, &["user", "role", "state"]).await)?;
    let mut users: Vec<RecordId> = assigned
        .iter()
        .filter(|r| idv(&r[1]) == Some(admin_role) && text(&r[2]).as_deref() == Some("active"))
        .filter_map(|r| idv(&r[0]))
        .collect();
    users.sort();
    users.dedup();
    let mut enabled = Vec::new();
    for user in users {
        let entity = st(nm(doctype::USER))?;
        if let Some(loaded) = st(tx.load(&entity, user, RowLock::None).await)?
            && text(&field(&loaded.data, "state")).as_deref() == Some("active")
        {
            enabled.push(user);
        }
    }
    Ok(enabled)
}

/// One user as the resolution reads it, or `None` when the record does not exist or its type is
/// unknown (fail closed: the resolution then denies).
///
/// # Errors
///
/// [`Unavail`] when a read fails.
pub async fn directory_user<T: Transaction>(
    tx: &mut T,
    id: RecordId,
    admins: &[RecordId],
) -> Result<Option<DirectoryUser>, Unavail> {
    let entity = st(nm(doctype::USER))?;
    let Some(loaded) = st(tx.load(&entity, id, RowLock::None).await)? else {
        return Ok(None);
    };
    let data = &loaded.data;
    let Some(user_type) = text(&field(data, "user_type"))
        .as_deref()
        .and_then(user_type_of)
    else {
        return Ok(None);
    };
    let sealed = |name: &str| !matches!(field(data, &spot_meta::sealed_column(name)), Value::Null);
    let mut second_factors = Vec::new();
    if sealed("totp_secret") {
        second_factors.push(SecondFactor::Totp);
    }
    let state = text(&field(data, "state")).unwrap_or_default();
    Ok(Some(DirectoryUser {
        id,
        enabled: state == "active",
        user_type,
        sign_in_name: text(&field(data, "user_name")).unwrap_or_default(),
        is_tenant_admin: state == "active" && admins.contains(&id),
        has_local_password: sealed("password_hash"),
        second_factors,
        // No per-user opt-out exists in the user record yet: single sign-on is allowed for every
        // user the tenant policy lets in.
        allows_sso: true,
    }))
}

/// The snapshot for `identity`.
///
/// # Errors
///
/// [`Unavail`] when any read fails (the sign-in is then refused).
pub async fn snapshot_in<T: Transaction>(
    tx: &mut T,
    env: &SnapshotEnv<'_>,
    identity: &SsoIdentity,
) -> Result<DirectorySnapshot, Unavail> {
    let mut snapshot = DirectorySnapshot {
        policy: Some(env.policy.clone()),
        providers: env.providers.clone(),
        ..DirectorySnapshot::default()
    };
    let admins = enabled_admins(tx).await?;
    snapshot.tenant_admins = admins.len();
    snapshot.break_glass_admins = admins
        .iter()
        .filter(|a| env.policy.break_glass.contains(a))
        .count();

    let mut reach: Vec<RecordId> = Vec::new();

    // Linked identities: the (issuer, subject) pair.
    let linked = eq_ids(
        tx,
        doctype::LINKED_IDENTITY,
        &[
            ("provider", &identity.issuer),
            ("subject", &identity.subject),
        ],
    )
    .await?;
    for id in linked {
        let entity = st(nm(doctype::LINKED_IDENTITY))?;
        if let Some(loaded) = st(tx.load(&entity, id, RowLock::None).await)?
            && let Some(user) = idv(&field(&loaded.data, "user"))
        {
            snapshot
                .linked
                .push(((identity.issuer.clone(), identity.subject.clone()), user));
            reach.push(user);
        }
    }

    // The e-mail: users who sign in with it, and invitations addressed to it. Only reported.
    if let Some(email) = identity.email.as_deref().map(str::to_ascii_lowercase)
        && !email.is_empty()
    {
        let mut by_email = eq_ids(tx, doctype::USER, &[("user_name", &email)]).await?;
        by_email.extend(eq_ids(tx, doctype::USER, &[("email", &email)]).await?);
        by_email.sort();
        by_email.dedup();
        for id in by_email {
            snapshot.emails.push((email.clone(), id));
            reach.push(id);
        }
        for id in eq_ids(tx, "invitation", &[("email", &email)]).await? {
            let entity = st(nm("invitation"))?;
            let Some(loaded) = st(tx.load(&entity, id, RowLock::None).await)? else {
                continue;
            };
            let open = text(&field(&loaded.data, "state")).as_deref() == Some(PENDING)
                && at(&field(&loaded.data, "expires_at")).is_some_and(|t| env.now < t);
            if open {
                snapshot.invitations.push((email.clone(), id));
                if let Some(user) = idv(&field(&loaded.data, "user")) {
                    reach.push(user);
                }
            }
        }
    }

    reach.sort();
    reach.dedup();
    for id in reach {
        if let Some(user) = directory_user(tx, id, &admins).await? {
            snapshot.users.push(user);
        }
    }
    Ok(snapshot)
}

/// A lock on a record for the rest of the transaction (used by the apply step).
pub(crate) async fn lock<T: Transaction>(
    tx: &mut T,
    entity: &str,
    id: RecordId,
) -> Result<Option<spot_storage_contract::Document>, Unavail> {
    let entity = st(nm(entity))?;
    st(tx.load(&entity, id, RowLock::Update(Wait::Block)).await)
}
