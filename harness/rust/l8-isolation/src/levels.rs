//! What each claimed isolation level forbids.
//!
//! The mapping is a documented table, not a silent choice. It follows Adya's phenomena
//! (Adya, Liskov and O'Neil, *Generalized Isolation Level Definitions*, ICDE 2000) and the
//! meaning the storage contract states for each level (`contracts/storage/src/tx.rs`, ADR-46):
//!
//! | Claimed level | Forbids |
//! | --- | --- |
//! | `ReadCommitted` | G0, G1a, G1b, G1c, a stale read, an internal inconsistency |
//! | `RepeatableRead` | the above, plus G-single, lost update, and a non-repeatable read (PostgreSQL's snapshot isolation forbids all three; it **permits** write skew, G2) |
//! | `Serializable` | everything, including G2 write skew |
//!
//! Two of these are ours, not Adya's, and are called out:
//!
//! - **Stale read.** Every declared level promises a read sees the latest committed data
//!   (read committed: the latest committed at statement start; repeatable read and serializable:
//!   one snapshot no older than the transaction's start). A committed read that misses a write
//!   committed before the reader started breaks that at any level, so it is forbidden
//!   everywhere.
//! - **Non-repeatable read.** Reading a key twice in one transaction and seeing it change with
//!   no write of its own between is exactly what read committed *allows* and repeatable read
//!   forbids, so it is forbidden at repeatable read and above only.
//!
//! An **internal inconsistency** (a transaction that cannot see its own writes, or reads that
//! cannot be ordered) is a self-contradiction, not a level property, and is forbidden
//! everywhere.

use std::collections::BTreeSet;

use spot_l7_invariants::history::Level;

use crate::anomaly::AnomalyClass;

/// The levels from strongest to weakest.
pub const LEVELS_STRONGEST_FIRST: [Level; 3] = [
    Level::Serializable,
    Level::RepeatableRead,
    Level::ReadCommitted,
];

/// What every level forbids.
const BASE: &[AnomalyClass] = &[
    AnomalyClass::G0,
    AnomalyClass::G1a,
    AnomalyClass::G1b,
    AnomalyClass::G1c,
    AnomalyClass::StaleRead,
    AnomalyClass::InternalInconsistency,
];

/// What repeatable read adds to [`BASE`].
const REPEATABLE_READ: &[AnomalyClass] = &[
    AnomalyClass::GSingle,
    AnomalyClass::LostUpdate,
    AnomalyClass::NonRepeatableRead,
];

/// What serializable adds to [`REPEATABLE_READ`].
const SERIALIZABLE: &[AnomalyClass] = &[AnomalyClass::G2];

/// The anomaly classes a claimed level forbids.
pub fn anomalies_forbidden(level: Level) -> Vec<AnomalyClass> {
    let mut out = BASE.to_vec();
    match level {
        Level::ReadCommitted => {}
        Level::RepeatableRead => out.extend_from_slice(REPEATABLE_READ),
        Level::Serializable => {
            out.extend_from_slice(REPEATABLE_READ);
            out.extend_from_slice(SERIALIZABLE);
        }
    }
    out
}

/// Whether a claimed level forbids a class.
pub fn is_forbidden(level: Level, class: AnomalyClass) -> bool {
    anomalies_forbidden(level).contains(&class)
}

/// The name used on the command line and in reports.
pub fn level_name(level: Level) -> &'static str {
    match level {
        Level::ReadCommitted => "rc",
        Level::RepeatableRead => "rr",
        Level::Serializable => "ser",
    }
}

/// A short description of the level.
pub fn level_describe(level: Level) -> &'static str {
    match level {
        Level::ReadCommitted => "ReadCommitted",
        Level::RepeatableRead => "RepeatableRead",
        Level::Serializable => "Serializable",
    }
}

/// Parses `rc`, `rr` or `ser` (and the full names).
pub fn parse_level(s: &str) -> Option<Level> {
    match s.to_ascii_lowercase().as_str() {
        "rc" | "readcommitted" | "read_committed" | "read-committed" => Some(Level::ReadCommitted),
        "rr" | "repeatableread" | "repeatable_read" | "repeatable-read" => {
            Some(Level::RepeatableRead)
        }
        "ser" | "serializable" => Some(Level::Serializable),
        _ => None,
    }
}

/// The strongest level whose forbidden set does not intersect `found`, or `None` when even read
/// committed is violated. This is the "strongest level the history is consistent with".
pub fn strongest_consistent(found: &BTreeSet<AnomalyClass>) -> Option<Level> {
    LEVELS_STRONGEST_FIRST.into_iter().find(|level| {
        anomalies_forbidden(*level)
            .iter()
            .all(|c| !found.contains(c))
    })
}
