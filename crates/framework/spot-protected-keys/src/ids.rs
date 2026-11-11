//! The identifiers of the key hierarchy: a tenant, a protection group, a node and the
//! monotonic key epoch.

use crate::error::ProtectedKeyError;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The longest an id or name may be.
const MAX_ID: usize = 128;

fn check(kind: &'static str, raw: &str) -> Result<(), ProtectedKeyError> {
    if raw.is_empty() || raw.len() > MAX_ID || raw.chars().any(|c| c.is_control()) {
        return Err(ProtectedKeyError::BadId { kind });
    }
    Ok(())
}

macro_rules! id_type {
    ($name:ident, $kind:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validates `raw` and wraps it.
            pub fn new(raw: &str) -> Result<Self, ProtectedKeyError> {
                check($kind, raw)?;
                Ok(Self(raw.to_owned()))
            }

            /// The text form.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(
    TenantId,
    "tenant id",
    "A tenant. The root key is scoped to it."
);
id_type!(
    GroupName,
    "group name",
    "A protection group (ADR-070 section 12), for example `bank_details`."
);
id_type!(
    NodeId,
    "node id",
    "A branch or offline node (ADR-070 rule 12.13 h)."
);

/// A key epoch. Epochs start at [`KeyEpoch::FIRST`] and only ever increase, so a revoked node
/// can never be handed a key of an epoch at or after its revocation epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyEpoch(u64);

impl KeyEpoch {
    /// The first epoch of a group.
    pub const FIRST: KeyEpoch = KeyEpoch(1);

    /// Wraps `value`. Zero is the absent epoch and is never issued.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw counter.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// The next epoch, saturating at the top of the counter.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// The next epoch, or `None` at the top of the counter.
    #[must_use]
    pub fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl fmt::Display for KeyEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn rejects_empty_and_control_ids() {
        assert!(GroupName::new("").is_err());
        assert!(GroupName::new("bank\u{0}details").is_err());
        assert!(NodeId::new("branch-1").is_ok());
        assert!(TenantId::new(&"x".repeat(MAX_ID + 1)).is_err());
    }

    #[test]
    fn epochs_only_increase() {
        assert_eq!(KeyEpoch::FIRST.get(), 1);
        assert_eq!(KeyEpoch::FIRST.next(), KeyEpoch::new(2));
        assert!(KeyEpoch::new(2) > KeyEpoch::FIRST);
        assert_eq!(KeyEpoch::new(u64::MAX).checked_next(), None);
    }
}
