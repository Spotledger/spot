//! What a hold belongs to: a migration, or a request (ADR-080 section 6.7).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hold::{MigrationId, REQUEST_KEY_PREFIX};

/// The longest request id accepted.
pub const MAX_REQUEST_ID_LEN: usize = 128;

/// The request a hold belongs to: the Case of the process engine whose region is held.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    /// Validates and wraps a request id: 1 to 128 characters of ASCII letters, digits, `_`, `-`
    /// and `.`. The set excludes `:` and whitespace, so a ledger key built from it is never
    /// ambiguous.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] when empty, too long or carrying another character.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        let bad = |reason: &str| Error::InvalidInput {
            what: "request id".to_owned(),
            reason: reason.to_owned(),
        };
        if id.is_empty() {
            return Err(bad("must not be empty"));
        }
        if id.len() > MAX_REQUEST_ID_LEN {
            return Err(bad("is longer than 128 characters"));
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Err(bad("may hold only ASCII letters, digits, `_`, `-` and `.`"));
        }
        Ok(Self(id))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What owns a hold. A migration hold is per site and has the `unverified` placeholder; a
/// request hold is per request, has no such state, and many may hold the same class at once.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HoldScope {
    /// A migration (WP-MIG-08).
    Migration(MigrationId),
    /// A request (WP-BPM-15).
    Request(RequestId),
}

impl HoldScope {
    /// The key the ledger stores for this scope. A migration keeps its own id, so every existing
    /// row and entry point is unchanged; a request lives in the reserved `request:` namespace,
    /// which no migration can use.
    #[must_use]
    pub fn ledger_key(&self) -> MigrationId {
        match self {
            HoldScope::Migration(id) => id.clone(),
            HoldScope::Request(id) => MigrationId::request_key(id),
        }
    }

    /// The scope a ledger key names. A key in the request namespace whose remainder is not a
    /// valid request id cannot have been written by this crate; it is read as a migration so a
    /// migration release never touches it.
    #[must_use]
    pub fn from_key(key: &MigrationId) -> Self {
        key.as_str()
            .strip_prefix(REQUEST_KEY_PREFIX)
            .and_then(|rest| RequestId::new(rest).ok())
            .map_or_else(|| HoldScope::Migration(key.clone()), HoldScope::Request)
    }

    /// Whether this is a request scope.
    #[must_use]
    pub fn is_request(&self) -> bool {
        matches!(self, HoldScope::Request(_))
    }
}

impl From<MigrationId> for HoldScope {
    fn from(id: MigrationId) -> Self {
        HoldScope::Migration(id)
    }
}

impl From<RequestId> for HoldScope {
    fn from(id: RequestId) -> Self {
        HoldScope::Request(id)
    }
}

impl fmt::Display for HoldScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HoldScope::Migration(id) => write!(f, "migration {id}"),
            HoldScope::Request(id) => write!(f, "request {id}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_bounded_and_plain() {
        assert!(RequestId::new("case_1.a-b").is_ok());
        assert!(RequestId::new("").is_err());
        assert!(RequestId::new("a:b").is_err());
        assert!(RequestId::new("a b").is_err());
        assert!(RequestId::new("x".repeat(129)).is_err());
        assert!(RequestId::new("x".repeat(128)).is_ok());
    }

    #[test]
    fn scope_round_trips_through_the_ledger_key() {
        let request = HoldScope::Request(RequestId::new("r1").unwrap());
        assert_eq!(HoldScope::from_key(&request.ledger_key()), request);
        let migration = HoldScope::Migration(MigrationId::new("mig-1").unwrap());
        assert_eq!(HoldScope::from_key(&migration.ledger_key()), migration);
        assert!(request.ledger_key().is_request_key());
        assert!(!migration.ledger_key().is_request_key());
    }

    #[test]
    fn a_malformed_request_key_is_read_as_a_migration() {
        let odd = MigrationId::new("request:bad id").unwrap();
        assert!(matches!(HoldScope::from_key(&odd), HoldScope::Migration(_)));
    }
}
