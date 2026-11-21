//! Byte-level helpers of the canonical encoding documented in the crate root.

/// Appends `bytes` as a length-prefixed field.
pub(crate) fn put_field(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Appends an optional field: `0`, or `1` then the field.
pub(crate) fn put_opt_field(out: &mut Vec<u8>, bytes: Option<&[u8]>) {
    match bytes {
        None => out.push(0),
        Some(b) => {
            out.push(1);
            put_field(out, b);
        }
    }
}
