//! The grant rules and the authorization decision (ADR-061 s7 item 2, answer to question 5).
//!
//! * `journal_reader` is **off by default**: nobody holds it until a tenant administrator grants
//!   it to a named principal.
//! * A grant has a maximum lifetime (`max_grant_days`, default 30, hard cap 365); an external
//!   principal's new grant is at most 7 days and is extended only by an explicit renewal.
//! * Only a tenant administrator grants, renews, revokes or lists. A holder of `journal_reader`
//!   cannot grant it.
//! * A grant is void when its grantor is no longer a tenant administrator.
//! * A revoked or expired grant is refused at the next request: no decision is cached longer
//!   than [`CACHE_MILLIS`], and a revoke drops the cached decisions of the grantee on this node.
//! * The tenant administrator reads through the same audited path, as `grant = admin`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::AccessError;
use crate::model::{
    ADMIN_GRANT, CACHE_MILLIS, DAY_MILLIS, EXTERNAL_MAX_DAYS, Fut, Grant, GrantAction, GrantEvent,
    GrantState, Settings, valid_id,
};
use crate::store::GrantStore;

/// Whether a principal is a tenant administrator **now**.
pub trait AdminDirectory: Send + Sync {
    /// The answer for `principal`; an error means the directory cannot answer (fail closed).
    fn is_tenant_administrator<'a>(&'a self, principal: &'a str) -> Fut<'a, bool>;
}

/// Where fresh ids come from (a library never reads randomness).
pub trait IdSource: Send + Sync {
    /// A new grant id.
    fn new_id(&self) -> String;
}

/// Why a caller has no usable grant. The audit record names this; the wire does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenyReason {
    /// Never granted.
    NoGrant,
    /// The grant ran out.
    Expired,
    /// An administrator revoked it.
    Revoked,
    /// The grantor is no longer an administrator, so the grant is void.
    GrantorNotAdministrator,
    /// The clock could not be read.
    ClockUnreadable,
}

impl DenyReason {
    /// The bounded code the audit record carries.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NoGrant => "no_grant",
            Self::Expired => "grant_expired",
            Self::Revoked => "grant_revoked",
            Self::GrantorNotAdministrator => "grantor_not_administrator",
            Self::ClockUnreadable => "clock_unreadable",
        }
    }
}

/// How [`Access::authorize`] can fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorizeError {
    /// No usable grant.
    Denied(DenyReason),
    /// The grant store or the administrator directory cannot answer.
    Unavailable,
}

/// What authorized a read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantRef {
    /// The tenant administrator, through the same audited path.
    Admin,
    /// A `journal_reader` grant.
    Grant(String),
}

impl GrantRef {
    /// The text the audit record's `grant` field holds.
    #[must_use]
    pub fn audit_text(&self) -> String {
        match self {
            Self::Admin => ADMIN_GRANT.to_owned(),
            Self::Grant(id) => id.clone(),
        }
    }
}

/// A caller the access surface may serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedReader {
    /// The caller.
    pub principal: String,
    /// What authorized the caller.
    pub grant: GrantRef,
}

struct CacheEntry {
    reader: AuthorizedReader,
    valid_until: i64,
}

/// The grant model over a [`GrantStore`].
pub struct Access {
    store: Arc<dyn GrantStore>,
    admins: Arc<dyn AdminDirectory>,
    ids: Arc<dyn IdSource>,
    settings: Settings,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl Access {
    /// The grant model.
    ///
    /// # Errors
    /// [`AccessError::SettingsInvalid`] when `settings` break their bounds.
    pub fn new(
        store: Arc<dyn GrantStore>,
        admins: Arc<dyn AdminDirectory>,
        ids: Arc<dyn IdSource>,
        settings: Settings,
    ) -> Result<Self, AccessError> {
        settings.validate()?;
        Ok(Self {
            store,
            admins,
            ids,
            settings,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// The settings in force.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, CacheEntry>> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn forget(&self, grantee: &str) {
        self.cache().remove(grantee);
    }

    /// Decides whether `principal` may read at `now_millis`.
    ///
    /// # Errors
    /// [`AuthorizeError::Denied`] with the reason, or [`AuthorizeError::Unavailable`] when a
    /// dependency cannot answer (the read is then refused, never allowed).
    pub async fn authorize(
        &self,
        principal: &str,
        now_millis: i64,
    ) -> Result<AuthorizedReader, AuthorizeError> {
        if let Some(entry) = self.cache().get(principal)
            && entry.valid_until > now_millis
        {
            return Ok(entry.reader.clone());
        }
        let is_admin = self
            .admins
            .is_tenant_administrator(principal)
            .await
            .map_err(|_| AuthorizeError::Unavailable)?;
        if is_admin {
            let reader = AuthorizedReader {
                principal: principal.to_owned(),
                grant: GrantRef::Admin,
            };
            self.remember(principal, &reader, now_millis, i64::MAX);
            return Ok(reader);
        }
        let grants = self
            .store
            .for_grantee(principal)
            .await
            .map_err(|_| AuthorizeError::Unavailable)?;
        let mut saw_expired = false;
        let mut saw_revoked = false;
        let mut saw_void = false;
        for grant in &grants {
            match grant.state(now_millis) {
                GrantState::Live => {
                    let grantor_ok = self
                        .admins
                        .is_tenant_administrator(&grant.grantor)
                        .await
                        .map_err(|_| AuthorizeError::Unavailable)?;
                    if grantor_ok {
                        let reader = AuthorizedReader {
                            principal: principal.to_owned(),
                            grant: GrantRef::Grant(grant.id.clone()),
                        };
                        self.remember(principal, &reader, now_millis, grant.expires_at_millis);
                        return Ok(reader);
                    }
                    saw_void = true;
                }
                GrantState::Expired => saw_expired = true,
                GrantState::Revoked => saw_revoked = true,
            }
        }
        // Most informative first, for the audit record; the wire shows only "expired" or not.
        let reason = if saw_void {
            DenyReason::GrantorNotAdministrator
        } else if saw_expired {
            DenyReason::Expired
        } else if saw_revoked {
            DenyReason::Revoked
        } else {
            DenyReason::NoGrant
        };
        Err(AuthorizeError::Denied(reason))
    }

    fn remember(&self, principal: &str, reader: &AuthorizedReader, now: i64, ends_at: i64) {
        let valid_until = now.saturating_add(CACHE_MILLIS).min(ends_at);
        self.cache().insert(
            principal.to_owned(),
            CacheEntry {
                reader: reader.clone(),
                valid_until,
            },
        );
    }

    async fn require_admin(&self, by: &str) -> Result<(), AccessError> {
        match self.admins.is_tenant_administrator(by).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(AccessError::NotAdministrator),
            Err(_) => Err(AccessError::Store),
        }
    }

    async fn refuse(&self, by: &str, grantee: &str, external: bool, reason: &str) {
        let event = GrantEvent {
            action: GrantAction::Refused,
            grantee: grantee.to_owned(),
            grantor: by.to_owned(),
            grant: self.ids.new_id(),
            expires_at_millis: None,
            external,
            reason: Some(reason.to_owned()),
        };
        // A refusal is audited best-effort: the caller already gets the refusal.
        let _ = self.store.record_event(&event).await;
    }

    /// Grants `journal_reader` to `grantee` for `days` days (the default is the maximum for an
    /// internal principal and 7 for an external one).
    ///
    /// # Errors
    /// [`AccessError::NotAdministrator`], [`AccessError::GrantInvalid`],
    /// [`AccessError::LifetimeExceeded`], [`AccessError::GrantExists`] or [`AccessError::Store`].
    pub async fn grant(
        &self,
        by: &str,
        grantee: &str,
        external: bool,
        days: Option<u32>,
        now_millis: i64,
    ) -> Result<Grant, AccessError> {
        if !valid_id(by) || !valid_id(grantee) {
            return Err(AccessError::GrantInvalid("an id is empty or too long"));
        }
        if let Err(error) = self.require_admin(by).await {
            if error == AccessError::NotAdministrator {
                self.refuse(by, grantee, external, "not_administrator")
                    .await;
            }
            return Err(error);
        }
        if by == grantee {
            return Err(AccessError::GrantInvalid(
                "an administrator reads as administrator and needs no grant",
            ));
        }
        let limit = if external {
            EXTERNAL_MAX_DAYS.min(self.settings.max_grant_days)
        } else {
            self.settings.max_grant_days
        };
        let days = days.unwrap_or(limit);
        if days == 0 {
            return Err(AccessError::GrantInvalid(
                "the lifetime is at least one day",
            ));
        }
        if days > limit {
            self.refuse(by, grantee, external, "lifetime_exceeded")
                .await;
            return Err(AccessError::LifetimeExceeded { limit_days: limit });
        }
        let existing = self.store.for_grantee(grantee).await?;
        if existing
            .iter()
            .any(|g| g.state(now_millis) == GrantState::Live)
        {
            return Err(AccessError::GrantExists);
        }
        let expires = now_millis.saturating_add(i64::from(days) * DAY_MILLIS);
        let grant = Grant {
            id: self.ids.new_id(),
            grantee: grantee.to_owned(),
            grantor: by.to_owned(),
            expires_at_millis: expires,
            external,
            created_at_millis: now_millis,
            revoked_at_millis: None,
            revoked_by: None,
        };
        let event = GrantEvent {
            action: GrantAction::Granted,
            grantee: grantee.to_owned(),
            grantor: by.to_owned(),
            grant: grant.id.clone(),
            expires_at_millis: Some(expires),
            external,
            reason: None,
        };
        self.store.insert(&grant, &event).await?;
        self.forget(grantee);
        Ok(grant)
    }

    /// Renews a grant: its new expiry is `days` from now, at most `max_grant_days`. An external
    /// grant is extended only here, by an administrator, explicitly.
    ///
    /// # Errors
    /// [`AccessError::NotAdministrator`], [`AccessError::GrantInvalid`],
    /// [`AccessError::LifetimeExceeded`], [`AccessError::GrantUnknown`] or [`AccessError::Store`].
    pub async fn renew(
        &self,
        by: &str,
        grant_id: &str,
        days: u32,
        now_millis: i64,
    ) -> Result<Grant, AccessError> {
        if !valid_id(by) || !valid_id(grant_id) {
            return Err(AccessError::GrantInvalid("an id is empty or too long"));
        }
        self.require_admin(by).await?;
        if days == 0 {
            return Err(AccessError::GrantInvalid(
                "the lifetime is at least one day",
            ));
        }
        if days > self.settings.max_grant_days {
            return Err(AccessError::LifetimeExceeded {
                limit_days: self.settings.max_grant_days,
            });
        }
        let current = self
            .store
            .get(grant_id)
            .await?
            .filter(|g| g.revoked_at_millis.is_none())
            .ok_or(AccessError::GrantUnknown)?;
        let expires = now_millis.saturating_add(i64::from(days) * DAY_MILLIS);
        let event = GrantEvent {
            action: GrantAction::Renewed,
            grantee: current.grantee.clone(),
            grantor: by.to_owned(),
            grant: current.id.clone(),
            expires_at_millis: Some(expires),
            external: current.external,
            reason: None,
        };
        let updated = self.store.renew(grant_id, expires, &event).await?;
        self.forget(&updated.grantee);
        Ok(updated)
    }

    /// Revokes a grant. The grantee's cached decision on this node is dropped at once.
    ///
    /// # Errors
    /// [`AccessError::NotAdministrator`], [`AccessError::GrantInvalid`],
    /// [`AccessError::GrantUnknown`] or [`AccessError::Store`].
    pub async fn revoke(
        &self,
        by: &str,
        grant_id: &str,
        now_millis: i64,
    ) -> Result<Grant, AccessError> {
        if !valid_id(by) || !valid_id(grant_id) {
            return Err(AccessError::GrantInvalid("an id is empty or too long"));
        }
        self.require_admin(by).await?;
        let current = self
            .store
            .get(grant_id)
            .await?
            .filter(|g| g.revoked_at_millis.is_none())
            .ok_or(AccessError::GrantUnknown)?;
        let event = GrantEvent {
            action: GrantAction::Revoked,
            grantee: current.grantee.clone(),
            grantor: by.to_owned(),
            grant: current.id.clone(),
            expires_at_millis: None,
            external: current.external,
            reason: None,
        };
        let updated = self.store.revoke(grant_id, by, now_millis, &event).await?;
        self.forget(&updated.grantee);
        Ok(updated)
    }

    /// Every grant, newest first. Administrator only.
    ///
    /// # Errors
    /// [`AccessError::NotAdministrator`] or [`AccessError::Store`].
    pub async fn list(&self, by: &str) -> Result<Vec<Grant>, AccessError> {
        self.require_admin(by).await?;
        self.store.list().await
    }
}
