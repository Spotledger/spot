//! Where a new assignment's id comes from.
//!
//! An [`Assignment`](crate::Assignment) is stored as a record, and a record's id is the
//! platform's record id (UUIDv7, time-ordered; ADR-33). This crate is pure: it reads no clock
//! and holds no random source, so the caller injects the id source and the id a grant carries
//! is exactly the one the platform would store. Tests inject a counting source, which keeps a
//! plan deterministic.

/// A source of assignment ids, in the record id's text form.
pub trait IdSource {
    /// The next id. Time-ordered in production, counting or scripted in tests.
    fn next(&mut self) -> String;
}
