//! Percent-encoding of path segments and the DocType of a resource path.

/// Percent-encodes one path segment (everything but RFC 3986 unreserved characters).
pub(crate) fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        let decoded = match (bytes.get(i), bytes.get(i + 1), bytes.get(i + 2)) {
            (Some(b'%'), Some(&h), Some(&l)) => hex(h)
                .zip(hex(l))
                .and_then(|(h, l)| u8::try_from(h * 16 + l).ok()),
            _ => None,
        };
        if let Some(byte) = decoded {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes.get(i).copied().unwrap_or(b'?'));
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `(doctype, id)` a `/api/v1/resource/...` path names; `id` is empty for a collection path.
pub(crate) fn resource_of(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/api/v1/resource/")?;
    let rest = rest.split('?').next().unwrap_or("");
    let mut segments = rest.split('/');
    let doctype = decode(segments.next().filter(|s| !s.is_empty())?);
    let id = segments.next().map(decode).unwrap_or_default();
    Some((doctype, id))
}
