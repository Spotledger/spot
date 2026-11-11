//! The standing of an API key, read on **every use** and never cached (ADR-070 section 5.5):
//! deactivating the owner stops the key on the next request.

use serde::{Deserialize, Serialize};
use spot_auth::StoredApiKey;
use spot_iam_directory::{User, UserType, is_anonymised};

use crate::error::KEY_NOT_USABLE;
use crate::scope::{KeyScope, ScopeError};

/// The facts about a key's owner that decide whether the key may be used or created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerState {
    /// The owner's kind.
    pub user_type: UserType,
    /// Whether the owner is enabled.
    pub enabled: bool,
    /// Whether the owner has been anonymised.
    pub anonymised: bool,
}

impl OwnerState {
    /// The state read from a directory record.
    #[must_use]
    pub fn from_user(user: &User) -> OwnerState {
        OwnerState {
            user_type: user.user_type,
            enabled: user.enabled,
            anonymised: is_anonymised(user),
        }
    }
}

/// A stored key's non-secret facts. The secret and its digest are `spot-auth`'s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct KeyRecord {
    /// The public key id.
    pub key_id: String,
    /// The owner's user id.
    pub owner: String,
    /// What the key may do (and when it expires).
    pub scope: KeyScope,
    /// When the key was revoked (unix seconds), if it was. Revocation is final.
    #[serde(default)]
    pub revoked_at: Option<i64>,
}

impl KeyRecord {
    /// A record for a freshly generated key: its public id, its owner and its scope. The
    /// digest stays with the stored key; this crate never sees a secret.
    #[must_use]
    pub fn new(stored: &StoredApiKey, owner: &str, scope: KeyScope) -> KeyRecord {
        KeyRecord {
            key_id: stored.key_id.clone(),
            owner: owner.to_owned(),
            scope,
            revoked_at: None,
        }
    }
}

/// Whether a key may be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Standing {
    /// The key may be used.
    Active,
    /// The owner is disabled.
    OwnerDisabled,
    /// The owner is anonymised.
    OwnerAnonymised,
    /// The key has expired.
    Expired,
    /// The key was revoked.
    Revoked,
    /// The owner is not a service or internal user.
    OwnerNotService,
}

impl Standing {
    /// Whether the key may be used.
    #[must_use]
    pub fn is_active(self) -> bool {
        self == Standing::Active
    }

    /// A stable name for logs and the audit trail (never sent to the caller).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Standing::Active => "active",
            Standing::OwnerDisabled => "owner_disabled",
            Standing::OwnerAnonymised => "owner_anonymised",
            Standing::Expired => "expired",
            Standing::Revoked => "revoked",
            Standing::OwnerNotService => "owner_not_service",
        }
    }
}

/// The standing of `key` for `owner` at `now` (unix seconds). Call it on every request.
#[must_use]
pub fn standing(owner: &OwnerState, key: &KeyRecord, now: i64) -> Standing {
    if key.revoked_at.is_some() {
        return Standing::Revoked;
    }
    if key.scope.expires_at.is_some_and(|at| now >= at) {
        return Standing::Expired;
    }
    if !matches!(owner.user_type, UserType::Service | UserType::Internal) {
        return Standing::OwnerNotService;
    }
    if owner.anonymised {
        return Standing::OwnerAnonymised;
    }
    if !owner.enabled {
        return Standing::OwnerDisabled;
    }
    Standing::Active
}

/// May a key be created for this owner?
///
/// # Errors
///
/// [`ScopeError::OwnerType`] for a portal or agent owner, [`ScopeError::OwnerInactive`] for a
/// disabled or anonymised one.
pub fn check_new_key(owner: &OwnerState) -> Result<(), ScopeError> {
    if !matches!(owner.user_type, UserType::Service | UserType::Internal) {
        return Err(ScopeError::OwnerType);
    }
    if owner.anonymised || !owner.enabled {
        return Err(ScopeError::OwnerInactive);
    }
    Ok(())
}

/// What the login service returns for a presented key. A refusal carries one code and one
/// message whatever the reason, so it never reveals whether the key id or its owner exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    /// The key may be used.
    Authenticated,
    /// The key may not be used.
    Refused {
        /// Always `iamaccess.key_not_usable`.
        code: &'static str,
        /// Always the same text.
        message: &'static str,
    },
}

impl AuthOutcome {
    /// The message every refusal carries.
    pub const MESSAGE: &'static str = "The API key cannot be used.";

    /// The refusal for any reason, including an unknown key id.
    #[must_use]
    pub fn refused() -> AuthOutcome {
        AuthOutcome::Refused {
            code: KEY_NOT_USABLE,
            message: Self::MESSAGE,
        }
    }

    /// The outcome for a standing.
    #[must_use]
    pub fn from_standing(standing: Standing) -> AuthOutcome {
        if standing.is_active() {
            AuthOutcome::Authenticated
        } else {
            AuthOutcome::refused()
        }
    }

    /// Whether the key may be used.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        matches!(self, AuthOutcome::Authenticated)
    }
}
