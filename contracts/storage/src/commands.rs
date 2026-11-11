//! The command ledger record (v0.3.0, WP-DOC-04.T8).
//!
//! A command ledger answers a repeated command id with the original outcome instead of running
//! the command again. The document engine writes one [`CommandRecord`] in the operation's own
//! transaction, so the record exists if and only if the document change does. The three
//! methods that read, write and prune records are on [`Transaction`](crate::Transaction).
//!
//! # Retention
//!
//! Retention is a per-site setting, never a constant in a backend or in the library. Each
//! record carries its own [`CommandRecord::retain_until`], computed by the writer from the
//! retention policy of the caller's class at write time. Changing the setting later therefore
//! never shortens or extends a row that already exists, and pruning deletes by `retain_until`
//! only. Neither the contract nor a backend reads the wall clock: the writer passes
//! `recorded_at` and `retain_until`, and the pruner passes `before`.

use time::OffsetDateTime;

use crate::error::{ErrorKind, StorageError, StorageResult};

/// The longest `principal` or `command_id`, in bytes.
pub const MAX_COMMAND_KEY_BYTES: usize = 255;

/// The longest `outcome` JSON text, in bytes. The outcome is small by design: identity and
/// messages, never a document body or a secret.
pub const MAX_COMMAND_OUTCOME_BYTES: usize = 16 * 1024;

/// One stored command outcome, keyed by `(principal, command_id)` within one tenant database
/// (so the site is implicit and a command id never crosses sites).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRecord {
    /// The acting principal's stable id; two principals using one command id never see each
    /// other's record.
    pub principal: String,
    /// The command id the caller supplied.
    pub command_id: String,
    /// A SHA-256 (or equivalent) digest of the command's canonical form. The same key with a
    /// different fingerprint is a conflict, decided by the caller, not by storage.
    pub fingerprint: Vec<u8>,
    /// The stored outcome as JSON text: identity and messages only.
    pub outcome: String,
    /// When the record was written; passed in by the writer.
    pub recorded_at: OffsetDateTime,
    /// When the record may be pruned; computed by the writer from the retention policy of the
    /// caller's class at write time (never earlier than `recorded_at`).
    pub retain_until: OffsetDateTime,
}

impl CommandRecord {
    /// Builds a record, refusing an empty or oversized key, an empty fingerprint, an oversized
    /// outcome, or a `retain_until` before `recorded_at`.
    pub fn new(
        principal: impl Into<String>,
        command_id: impl Into<String>,
        fingerprint: Vec<u8>,
        outcome: impl Into<String>,
        recorded_at: OffsetDateTime,
        retain_until: OffsetDateTime,
    ) -> StorageResult<Self> {
        let record = Self {
            principal: principal.into(),
            command_id: command_id.into(),
            fingerprint,
            outcome: outcome.into(),
            recorded_at,
            retain_until,
        };
        record.validate()?;
        Ok(record)
    }

    /// Checks the rules of [`CommandRecord::new`]; a backend calls it in `command_put`.
    pub fn validate(&self) -> StorageResult<()> {
        let bad = |why: &str| Err(StorageError::new(ErrorKind::InvalidValue, why));
        if self.principal.is_empty() || self.principal.len() > MAX_COMMAND_KEY_BYTES {
            return bad("a command record needs a principal of 1 to 255 bytes");
        }
        if self.command_id.is_empty() || self.command_id.len() > MAX_COMMAND_KEY_BYTES {
            return bad("a command record needs a command id of 1 to 255 bytes");
        }
        if self.fingerprint.is_empty() {
            return bad("a command record needs a fingerprint");
        }
        if self.outcome.len() > MAX_COMMAND_OUTCOME_BYTES {
            return bad("a command outcome is limited to 16 KiB; store identity, not a body");
        }
        if self.retain_until < self.recorded_at {
            return bad("retain_until must not be before recorded_at");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    fn at() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    fn rec(retain: OffsetDateTime) -> StorageResult<CommandRecord> {
        CommandRecord::new("p", "c", vec![1], "{}", at(), retain)
    }

    #[test]
    fn accepts_a_valid_record_and_equal_instants() {
        assert!(rec(at() + Duration::days(7)).is_ok());
        assert!(rec(at()).is_ok());
    }

    #[test]
    fn refuses_retain_until_before_recorded_at() {
        assert!(matches!(
            rec(at() - Duration::seconds(1)),
            Err(e) if e.kind == ErrorKind::InvalidValue
        ));
    }

    #[test]
    fn refuses_bad_keys_fingerprint_and_outcome() {
        let r = at() + Duration::days(1);
        assert!(CommandRecord::new("", "c", vec![1], "{}", at(), r).is_err());
        assert!(CommandRecord::new("p", "", vec![1], "{}", at(), r).is_err());
        assert!(CommandRecord::new("p", "c", vec![], "{}", at(), r).is_err());
        let long = "x".repeat(MAX_COMMAND_KEY_BYTES + 1);
        assert!(CommandRecord::new(long, "c", vec![1], "{}", at(), r).is_err());
        let big = "x".repeat(MAX_COMMAND_OUTCOME_BYTES + 1);
        assert!(CommandRecord::new("p", "c", vec![1], big, at(), r).is_err());
    }
}
