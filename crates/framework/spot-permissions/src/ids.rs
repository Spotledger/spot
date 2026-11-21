//! Parsing a record id from its canonical text form, for model files.
//!
//! The storage contract's `RecordId` carries no text-construction API, and the brief's
//! dependency list has no uuid crate; this module is its inverse, written here as
//! `spot-activity`'s wire module also writes it: the 32 hex digits decoded onto 16 bytes,
//! re-checked as a UUIDv7 through [`RecordId::from_bytes`], and the text re-built to confirm
//! the input was the canonical lower-case hyphenated form.

use spot_app_api::prelude::RecordId;

/// The id its canonical text names, or the reason it is not one.
pub fn parse_record_id(text: &str) -> Result<RecordId, String> {
    let bytes = text.as_bytes();
    if bytes.len() != 36
        || bytes[8] != b'-'
        || bytes[13] != b'-'
        || bytes[18] != b'-'
        || bytes[23] != b'-'
    {
        return Err(format!(
            "`{text}` is not a record id: not canonical UUIDv7 text"
        ));
    }
    let mut raw = [0u8; 16];
    let mut di = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            continue;
        }
        let nib = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => return Err(format!("`{text}` is not a record id: not lower-case hex")),
        };
        if di.is_multiple_of(2) {
            raw[di / 2] = nib << 4;
        } else {
            raw[di / 2] |= nib;
        }
        di += 1;
    }
    let rebuilt =
        RecordId::from_bytes(raw).map_err(|e| format!("`{text}` is not a record id: {e}"))?;
    if rebuilt.to_string() != text {
        // A non-canonical (upper-case, or variant-wrong-but-decodable) form is refused so the
        // same id always serialises one way (blueprint §4: stable forms).
        return Err(format!("`{text}` is not a record id: not canonical"));
    }
    Ok(rebuilt)
}
