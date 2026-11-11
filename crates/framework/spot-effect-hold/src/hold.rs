//! The hold state machine per site and the migration identifier.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::scope::RequestId;

/// The migration a hold belongs to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MigrationId(String);

impl MigrationId {
    /// Validates and wraps a migration id: non-empty and printable.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] when empty or carrying a control character.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        if id.is_empty() {
            return Err(Error::InvalidInput {
                what: "migration id".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        if id.chars().any(char::is_control) {
            return Err(Error::InvalidInput {
                what: "migration id".to_owned(),
                reason: "must be printable".to_owned(),
            });
        }
        Ok(Self(id))
    }

    /// The id for effects held while the hold state itself is unknown. A fail-closed placeholder;
    /// such effects are re-driven once the state is known.
    #[must_use]
    pub fn unverified() -> Self {
        Self("unverified".to_owned())
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The ledger key of a request scope. Not validated as a migration id: it lives in the
    /// reserved `request:` namespace that [`MigrationId::is_request_key`] recognises.
    pub(crate) fn request_key(request: &RequestId) -> Self {
        Self(format!("{REQUEST_KEY_PREFIX}{}", request.as_str()))
    }

    /// Whether this key is in the reserved request namespace, so a ledger row under it belongs
    /// to a request scope and never to a migration.
    #[must_use]
    pub fn is_request_key(&self) -> bool {
        self.0.starts_with(REQUEST_KEY_PREFIX)
    }
}

/// The reserved prefix of a request scope's ledger key.
pub(crate) const REQUEST_KEY_PREFIX: &str = "request:";

impl fmt::Display for MigrationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for MigrationId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::new(s)
    }
}

/// The hold state machine: `Open -> Held -> Released | Discarded -> Open`.
///
/// `Released` and `Discarded` are open sites that remember what happened; a later `begin`
/// transitions them into `Held` again. Every transition is idempotent and keyed by migration id,
/// so a retry after a crash is safe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HoldState {
    /// The site admits external effects.
    Open,
    /// The site is held while `migration_id` is unverified.
    Held {
        /// The migration being verified.
        migration_id: MigrationId,
        /// Why the hold was taken.
        reason: String,
        /// Unix microseconds the hold was taken at.
        since: i64,
        /// When verification is expected to finish, if known.
        expected_by: Option<i64>,
    },
    /// Verification passed and the held effects were released to `migration_id`.
    Released {
        /// The migration that was verified.
        migration_id: MigrationId,
        /// Unix microseconds release finished at.
        at: i64,
    },
    /// Verification failed and the held effects were discarded for `migration_id`.
    Discarded {
        /// The migration that was rolled back.
        migration_id: MigrationId,
        /// Unix microseconds discard finished at.
        at: i64,
        /// Why.
        reason: String,
    },
}

impl HoldState {
    /// Whether the site admits external effects: `Open`, `Released` or `Discarded`.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.is_held()
    }

    /// Whether the site is currently held.
    #[must_use]
    pub fn is_held(&self) -> bool {
        matches!(self, HoldState::Held { .. })
    }

    /// The migration this state names, if any.
    #[must_use]
    pub fn migration_id(&self) -> Option<&MigrationId> {
        match self {
            HoldState::Open => None,
            HoldState::Held { migration_id, .. }
            | HoldState::Released { migration_id, .. }
            | HoldState::Discarded { migration_id, .. } => Some(migration_id),
        }
    }

    /// Whether this state belongs to `migration_id`.
    #[must_use]
    pub fn is_for(&self, migration_id: &MigrationId) -> bool {
        self.migration_id() == Some(migration_id)
    }
}
