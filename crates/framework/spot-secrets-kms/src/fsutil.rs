//! Reading a small private file (the token, the seed) with its mode checked.

use zeroize::Zeroizing;

/// Why a private file was refused. A fixed sentence, never the path or the content.
pub(crate) type FileRefusal = &'static str;

/// Reads `path`, which must be a regular file (a symlink is refused), at most `max_len` bytes,
/// and on Unix with no group or other permission bits.
///
/// Ownership is not checked: it needs the process's uid, which std does not expose without
/// `unsafe` or an extra dependency (see the README).
pub(crate) async fn read_private(
    path: &std::path::Path,
    max_len: u64,
) -> Result<Zeroizing<Vec<u8>>, FileRefusal> {
    let meta = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| "the file is missing or unreadable")?;
    if !meta.is_file() {
        return Err("the path is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err("the file mode allows group or other access");
        }
    }
    if meta.len() > max_len {
        return Err("the file is too large");
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| "the file is missing or unreadable")?;
    if bytes.len() as u64 > max_len {
        return Err("the file is too large");
    }
    Ok(Zeroizing::new(bytes))
}
