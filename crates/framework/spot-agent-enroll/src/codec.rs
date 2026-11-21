//! The wire codec: base64 text for the byte values of the exchange (keys, signatures,
//! nonces) and small helpers around it.
//!
//! Byte fields are never raw bytes in a message; they travel as standard base64 text.
//! Every decode path rejects a wrong alphabet or a wrong length with
//! [`EnrolError::CryptoMalformed`], and the offending text is never echoed.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::error::EnrolError;

/// Encodes `bytes` as lower-case hex (fingerprints).
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Encodes `bytes` as standard base64 text.
#[must_use]
pub fn enc64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decodes standard base64 `text` into exactly `N` bytes; a wrong alphabet, padding or
/// length is [`EnrolError::CryptoMalformed`]. The decode is **canonical**: base64
/// text whose trailing bits are not zero (a tampered alphabet character that decodes
/// to the same bytes) is refused, so a wire text maps to exactly one byte value.
pub fn dec64<const N: usize>(text: &str) -> Result<[u8; N], EnrolError> {
    let raw = STANDARD
        .decode(text)
        .map_err(|_| EnrolError::CryptoMalformed)?;
    let array: [u8; N] = raw
        .as_slice()
        .try_into()
        .map_err(|_| EnrolError::CryptoMalformed)?;
    // Canonicality: re-encode and compare. A text with non-zero trailing bits decodes
    // fine but is not what this crate would have emitted; refusing it keeps the wire
    // form injective (one text per byte value) and stops flip-in-padding forgeries.
    if STANDARD.encode(raw.as_slice()) != text {
        return Err(EnrolError::CryptoMalformed);
    }
    Ok(array)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{dec64, enc64};

    #[test]
    fn round_trip_and_reject() {
        let bytes = [7u8; 32];
        let text = enc64(&bytes);
        assert_eq!(text.len(), 44);
        assert_eq!(dec64::<32>(&text).unwrap(), bytes);
        // Wrong length (dropped character) and wrong alphabet are both refused.
        assert!(dec64::<32>(&text[..text.len() - 1]).is_err());
        assert!(dec64::<32>(&format!("{text}!")).is_err());
        let sig = enc64(&[0u8; 64]);
        assert_eq!(sig.len(), 88);
        assert!(dec64::<64>(&sig).is_ok());
        assert!(dec64::<64>(&enc64(&[0u8; 32])).is_err());
        // Non-canonical trailing bits: a text that decodes but re-encodes differently.
        let mut non_canonical = sig.clone();
        // The final data character carries six bits of which two are payload; swap it
        // for another character in the same 4-bit block by XORing the alphabet char.
        let data_chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let last = non_canonical.as_bytes()[85];
        let index = data_chars.iter().position(|c| *c == last).unwrap();
        let flipped = data_chars[index ^ 1];
        non_canonical.replace_range(85..86, &(flipped as char).to_string());
        assert!(
            dec64::<64>(&non_canonical).is_err(),
            "padding-bit flip must be refused"
        );
    }
}
