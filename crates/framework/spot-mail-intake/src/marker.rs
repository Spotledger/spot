//! The marker on mail this site sends (WP-PLT-11.T16): a keyed MAC over the message's own
//! Message-ID, carried in the `X-Spot-Outgoing` header.
//!
//! A message that comes back to a polled mailbox (a forward rule, a BCC, a mailing-list loop, an
//! answer machine copying the original) must not become a document, or the site would answer
//! itself forever. The `From` address check catches part of that; the marker catches mail the
//! site sent for any account.
//!
//! The marker is `v1.<64 hex>`: HMAC-SHA256 under a key derived from the site's mail secret,
//! over the normalised Message-ID. An outsider cannot make one, so they can neither get their
//! own mail dropped nor pass off a loop as theirs; the one thing a copied marker can do is get a
//! copy of the same mail (same Message-ID) dropped, which is what it already was.
//!
//! The key is derived, never the secret itself: `HMAC(mail secret, "spot-mail-own-marker-v1")`.
//! The sender (the outgoing queue) and the intake derive it from the same secret, so a rotated
//! secret makes older markers stop verifying (such mail is then handled as mail from outside,
//! which the From check and the auto-submitted check still guard).

use std::fmt;

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::poll::normalise_message_id;

type HmacSha256 = Hmac<Sha256>;

/// The header that carries the marker.
pub const HEADER: &str = "X-Spot-Outgoing";

const VERSION_PREFIX: &str = "v1.";
const KEY_CONTEXT: &[u8] = b"spot-mail-own-marker-v1";
const MAC_HEX_LEN: usize = 64;

/// The key that signs and verifies own-mail markers. Holds derived key material only; it never
/// prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct OwnMailKey([u8; 32]);

impl fmt::Debug for OwnMailKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnMailKey(..)")
    }
}

impl OwnMailKey {
    /// The key derived from the site's mail secret. `None` for an empty secret (a marker under
    /// an empty key would be forgeable by anyone).
    #[must_use]
    pub fn derive(mail_secret: &[u8]) -> Option<Self> {
        if mail_secret.is_empty() {
            return None;
        }
        let mut mac = HmacSha256::new_from_slice(mail_secret).ok()?;
        mac.update(KEY_CONTEXT);
        let bytes: [u8; 32] = mac.finalize().into_bytes().into();
        Some(Self(bytes))
    }

    /// The marker for a message with `message_id` (angle brackets and case are not significant),
    /// or `None` for an empty id. This is what the sender puts in the `X-Spot-Outgoing` header.
    #[must_use]
    pub fn marker(&self, message_id: &str) -> Option<String> {
        let tag = self.mac(message_id)?.finalize().into_bytes();
        let mut out = String::with_capacity(VERSION_PREFIX.len() + MAC_HEX_LEN);
        out.push_str(VERSION_PREFIX);
        for byte in tag.iter() {
            out.push_str(&format!("{byte:02x}"));
        }
        Some(out)
    }

    /// Whether `value` is a valid marker for `message_id` under this key. Compared in constant
    /// time; a malformed value, another version, an empty id or a wrong tag are all `false`.
    #[must_use]
    pub fn verifies(&self, message_id: &str, value: &str) -> bool {
        let Some(hex) = value.trim().strip_prefix(VERSION_PREFIX) else {
            return false;
        };
        let Some(provided) = decode_hex(hex) else {
            return false;
        };
        match self.mac(message_id) {
            Some(mac) => mac.verify_slice(&provided).is_ok(),
            None => false,
        }
    }

    fn mac(&self, message_id: &str) -> Option<HmacSha256> {
        let id = normalise_message_id(message_id);
        if id.is_empty() {
            return None;
        }
        let mut mac = HmacSha256::new_from_slice(&self.0).ok()?;
        mac.update(id.as_bytes());
        Some(mac)
    }
}

/// Decodes exactly 64 lower- or upper-case hex digits into 32 bytes.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != MAC_HEX_LEN || !text.is_ascii() {
        return None;
    }
    let digits = text.as_bytes();
    let mut out = Vec::with_capacity(MAC_HEX_LEN / 2);
    for pair in digits.chunks(2) {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        out.push(u8::try_from(high * 16 + low).ok()?);
    }
    Some(out)
}
