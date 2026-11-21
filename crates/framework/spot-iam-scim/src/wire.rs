//! The wire form of a [`RecordId`]: the canonical lower-case hyphenated UUID text.
//!
//! The SCIM resources carry the canonical id text; every lookup parses it back to a
//! [`RecordId`], so an id a client invents is refused rather than treated as a key.

use spot_storage_contract::RecordId;

/// Parses the canonical form: 36 characters, lower-case hex, hyphens at 8, 13, 18 and 23.
pub(crate) fn parse_record_id(text: &str) -> Result<RecordId, String> {
    let bad = || "a record id must be a lower-case hyphenated UUIDv7".to_owned();
    if text.len() != 36 {
        return Err(bad());
    }
    let mut bytes = [0_u8; 16];
    let (mut at, mut high) = (0_usize, None);
    for (i, c) in text.bytes().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if c != b'-' {
                return Err(bad());
            }
            continue;
        }
        let value = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => return Err(bad()),
        };
        match high.take() {
            None => high = Some(value),
            Some(h) => {
                if let Some(slot) = bytes.get_mut(at) {
                    *slot = (h << 4) | value;
                }
                at += 1;
            }
        }
    }
    RecordId::from_bytes(bytes).map_err(|e| e.to_string())
}
