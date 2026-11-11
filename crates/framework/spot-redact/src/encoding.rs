//! Byte encodings used to spot a registered secret in transit-shaped text.
//!
//! Only what the brief asks for: URL encoding. Base64 forms come from the
//! [`base64`](https://docs.rs/base64) engines in `known`.

/// URL-encodes every byte that is not an unreserved one (`A-Za-z0-9-._~`) as `%XX`,
/// upper-case hex (`%2F`, never `%2f` — both spellings match, this is the canonical one).
#[must_use]
pub fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(*b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}
