//! Finding a planted secret in a payload: exact, base64 and hex forms.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64, with padding.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(B64[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The forms of `secret` a payload must not contain: the value, its base64 (with and without
/// padding) and its hex (lower and upper case).
pub fn forms(secret: &str) -> Vec<(&'static str, String)> {
    let b64 = base64(secret.as_bytes());
    let h = hex(secret.as_bytes());
    vec![
        ("exact", secret.to_owned()),
        ("base64", b64.clone()),
        ("base64_unpadded", b64.trim_end_matches('=').to_owned()),
        ("hex", h.clone()),
        ("hex_upper", h.to_uppercase()),
    ]
}

/// The form of `secret` that `payload` contains, if any. A value shorter than 4 bytes is not
/// searched (it would match noise), and a partial prefix of a secret never counts.
pub fn find(payload: &str, secret: &str) -> Option<&'static str> {
    if secret.len() < 4 {
        return None;
    }
    forms(secret)
        .into_iter()
        .find(|(_, form)| !form.is_empty() && payload.contains(form.as_str()))
        .map(|(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn the_exact_base64_and_hex_forms_are_found() {
        let secret = "l7-secret-42";
        assert_eq!(find("x l7-secret-42 y", secret), Some("exact"));
        let b = base64(secret.as_bytes());
        assert_eq!(find(&format!("{{\"v\":\"{b}\"}}"), secret), Some("base64"));
        assert_eq!(find(&hex(secret.as_bytes()), secret), Some("hex"));
        assert_eq!(
            find(&hex(secret.as_bytes()).to_uppercase(), secret),
            Some("hex_upper")
        );
    }

    #[test]
    fn a_partial_prefix_and_a_short_value_do_not_count() {
        assert_eq!(find("l7-secret-", "l7-secret-42"), None);
        assert_eq!(find("abc", "abc"), None);
        assert_eq!(find("nothing here", "l7-secret-42"), None);
    }
}
