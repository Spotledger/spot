//! Deterministic record ids derived from text.
//!
//! The crate never reads the wall clock, so it never calls `RecordId::new_v7`. A created
//! document's id and a Case's graph node are derived from a stable string instead — the
//! idempotency key for a document, the Case id for the Case node — so the same input always
//! yields the same [`RecordId`], and a replay after a crash touches nothing new.

use spot_app_api::prelude::{Name, RecordId};
use spot_storage_contract::NodeRef;

use spot_bpm_engine::CaseId;

/// The record id a stable string names: a UUIDv7-shaped, deterministic id.
///
/// The id is derived from `text` with a fixed hash, so it is stable across runs and processes; it
/// is not random and carries no time.
#[must_use]
pub fn record_id_for(text: &str) -> RecordId {
    let mut bytes = hash128(text);
    // Version 7 in the high nibble of byte 6, RFC 4122 variant in byte 8, so the bytes are a
    // valid UUIDv7 and `RecordId::from_bytes` accepts them.
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    match RecordId::from_bytes(bytes) {
        Ok(id) => id,
        // Unreachable: the version and variant bits are set above.
        Err(_) => unreachable!("the derived bytes are a UUIDv7"),
    }
}

/// The business-graph node a Case occupies: the `bpm_case` entity with a deterministic id.
#[must_use]
pub fn case_node(case: &CaseId) -> NodeRef {
    NodeRef {
        entity: case_entity(),
        id: record_id_for(&format!("case:{}", case.as_str())),
    }
}

/// The entity name of a Case in the business graph.
///
/// # Panics
/// Never: `bpm_case` is a valid storage name.
#[must_use]
pub fn case_entity() -> Name {
    match Name::new("bpm_case") {
        Ok(name) => name,
        Err(_) => unreachable!("`bpm_case` is a valid storage name"),
    }
}

/// A hyphenated or compact UUID as a record id.
#[must_use]
pub fn parse_record_id(text: &str) -> Option<RecordId> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let part = hex.get(index * 2..index * 2 + 2)?;
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    RecordId::from_bytes(bytes).ok()
}

/// A 128-bit hash of `text`: two independent 64-bit FNV-1a folds, so distinct inputs give
/// distinct ids with no dependency on a hash crate.
fn hash128(text: &str) -> [u8; 16] {
    let mut low: u64 = 0xcbf2_9ce4_8422_2325;
    let mut high: u64 = 0x9e37_79b9_7f4a_7c15;
    for byte in text.as_bytes() {
        low ^= u64::from(*byte);
        low = low.wrapping_mul(0x0000_0100_0000_01b3);
        high = high.rotate_left(5) ^ u64::from(*byte);
        high = high.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&low.to_be_bytes());
    out[8..].copy_from_slice(&high.to_be_bytes());
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinct() {
        assert_eq!(record_id_for("a"), record_id_for("a"));
        assert_ne!(record_id_for("a"), record_id_for("b"));
        // A valid UUIDv7: the version nibble is 7.
        let id = record_id_for("case:one");
        assert_eq!(id.as_bytes()[6] >> 4, 7);
    }

    #[test]
    fn a_case_has_a_bpm_case_node() {
        let case = CaseId::new("case1").unwrap();
        let node = case_node(&case);
        assert_eq!(node.entity.as_str(), "bpm_case");
        assert_eq!(node.id, record_id_for("case:case1"));
    }
}
