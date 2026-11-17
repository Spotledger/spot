//! The command ledger of the fake (storage contract 0.3.0, WP-DOC-04.T8): committed records, and
//! what a transaction did to them. A record commits and rolls back with its transaction.

use std::collections::BTreeMap;

use spot_storage_contract::{CommandRecord, ErrorKind, StorageError, StorageResult};
use time::OffsetDateTime;

/// The key of a record: `(principal, command id)`. The store is one tenant, so the site is
/// implicit.
pub(crate) type Key = (String, String);

/// Committed command records.
#[derive(Debug, Default)]
pub(crate) struct CommandStore {
    rows: BTreeMap<Key, CommandRecord>,
}

impl CommandStore {
    /// The committed record, if any.
    pub fn get(&self, key: &Key) -> Option<&CommandRecord> {
        self.rows.get(key)
    }

    /// How many committed records a prune at `before` removes, not counting the ones the
    /// transaction's earlier prunes (`already`) removed.
    pub fn prunable(&self, before: OffsetDateTime, already: &[OffsetDateTime]) -> u64 {
        self.rows
            .values()
            .filter(|r| r.retain_until <= before && !pruned_by(r, already))
            .count() as u64
    }

    /// Applies a transaction's prunes and inserts at commit. Fails with `UniqueViolation` if
    /// another transaction committed the same key first (nothing is applied then).
    pub fn check_inserts(
        &self,
        inserts: &BTreeMap<Key, CommandRecord>,
        prunes: &[OffsetDateTime],
    ) -> StorageResult<()> {
        for key in inserts.keys() {
            if let Some(existing) = self.rows.get(key)
                && !pruned_by(existing, prunes)
            {
                return Err(duplicate(key));
            }
        }
        Ok(())
    }

    /// Applies the prunes, then the inserts.
    pub fn apply(&mut self, inserts: &BTreeMap<Key, CommandRecord>, prunes: &[OffsetDateTime]) {
        self.rows.retain(|_, r| !pruned_by(r, prunes));
        for (key, record) in inserts {
            self.rows.insert(key.clone(), record.clone());
        }
    }
}

/// Whether one of `prunes` (each a `before`) removes `record` (`retain_until <= before`).
pub(crate) fn pruned_by(record: &CommandRecord, prunes: &[OffsetDateTime]) -> bool {
    prunes.iter().any(|before| record.retain_until <= *before)
}

pub(crate) fn duplicate(key: &Key) -> StorageError {
    StorageError::new(
        ErrorKind::UniqueViolation,
        format!(
            "a command record for command `{}` of `{}` already exists",
            key.1, key.0
        ),
    )
}
