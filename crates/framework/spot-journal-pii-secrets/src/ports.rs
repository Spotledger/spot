//! The four things the adapter asks of its host, each a trait so the crate reads no clock, no
//! random source, no database and no audit table of its own.

use serde::{Deserialize, Serialize};
use spot_secrets::WrappedKey;

/// A failure of a host port. The text must describe the failure and never a key or a subject.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PortError(pub String);

/// The random source for new subject keys. Production passes an operating-system CSPRNG; the
/// library never draws randomness itself and never derives a key from a clock.
pub trait Rng: Send + Sync {
    /// Fills `buf` with random bytes, or fails (which fails the key's creation: a key is never
    /// made from a partial fill).
    fn fill(&self, buf: &mut [u8]) -> Result<(), PortError>;
}

/// The time source for the audit event and for the age of cached keys.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_unix_ms(&self) -> i64;
}

/// What a store holds under one entry id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredKey {
    /// A live key, wrapped by the tenant's key provider.
    Live(WrappedKey),
    /// The key was destroyed. The tombstone is what makes `key_for` answer `None` for good
    /// instead of minting a second key the old ciphertext cannot open.
    Destroyed,
}

/// Where wrapped subject keys live. The persistent table is a later task; the in-memory double
/// is [`crate::testing::MemoryWrappedKeyStore`].
///
/// Entry ids are opaque to the store. Every method may fail, and a failure is never "absent".
pub trait WrappedKeyStore: Send + Sync {
    /// The entry, or `None` when nothing was ever stored (or tombstoned) under `entry`.
    fn get(&self, entry: &str) -> Result<Option<StoredKey>, PortError>;

    /// Stores `wrapped` under `entry` unless the entry already exists in any state. Returns
    /// whether it stored (the loser of a race with another node gets `false`).
    fn put_if_absent(&self, entry: &str, wrapped: WrappedKey) -> Result<bool, PortError>;

    /// Deletes the wrapped key under `entry` and leaves a tombstone (also when nothing was
    /// there). Returns whether a live key was removed.
    fn destroy(&self, entry: &str) -> Result<bool, PortError>;
}

/// The audit event name, `journal_pii.subject_key_destroyed`.
///
/// It is an event name, registered in the `journal_pii` catalogue (class `none`, an audit event
/// and not an error). It is a plain literal: the error-code scan requires every catalogued name
/// to appear in a scanned source.
pub const SUBJECT_KEY_DESTROYED: &str = "journal_pii.subject_key_destroyed";

/// The one audit event this crate emits: a subject's key was destroyed.
///
/// `subject_hash` is the subject id hashed with the tenant salt (lower-case hex SHA-256); the
/// raw subject never appears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectKeyDestroyed {
    /// The tenant-salted hash of the data subject.
    pub subject_hash: String,
    /// When, in milliseconds since the Unix epoch, from the injected [`Clock`].
    pub at: i64,
    /// True when no live key existed (never created, or destroyed before).
    pub already_absent: bool,
}

/// Where audit events go. The adapter does not depend on a database: the host adapts this to
/// its audit log.
pub trait AuditPort: Send + Sync {
    /// Records `event` (named [`SUBJECT_KEY_DESTROYED`]), or fails.
    fn record(&self, event: &SubjectKeyDestroyed) -> Result<(), PortError>;
}

/// Told when a cached key has been wiped, with the bytes as they stand after the wipe. A test
/// hook: production passes none.
pub trait WipeObserver: Send + Sync {
    /// A cached key's bytes were just overwritten; `after_wipe` is what they hold now.
    fn wiped(&self, after_wipe: &[u8; 32]);
}
