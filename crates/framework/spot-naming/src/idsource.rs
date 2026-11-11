//! The id source for the `hash` strategy.
//!
//! A `hash` name is a short id derived from a UUIDv7 (ADR-33). The source is injected so a
//! test can make it deterministic; production uses [`UuidV7Source`].

use spot_storage_contract::RecordId;

/// A source of record ids for `hash` naming.
pub trait NamingIdSource: Send + Sync {
    /// The next id; time-ordered in production, fixed or scripted in tests.
    fn next_record_id(&self) -> RecordId;
}

/// The real source: a fresh UUIDv7, as every record id (ADR-33).
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7Source;

impl NamingIdSource for UuidV7Source {
    fn next_record_id(&self) -> RecordId {
        RecordId::new_v7()
    }
}

/// A source that always returns the same id, for deterministic tests.
#[derive(Clone, Copy, Debug)]
pub struct FixedIdSource {
    id: RecordId,
}

impl FixedIdSource {
    /// A source returning `id` every time.
    pub fn new(id: RecordId) -> Self {
        Self { id }
    }
}

impl NamingIdSource for FixedIdSource {
    fn next_record_id(&self) -> RecordId {
        self.id
    }
}
