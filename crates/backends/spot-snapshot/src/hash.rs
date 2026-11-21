//! Checksummed file operations shared by the providers: SHA-256 of a file, a verified
//! tree copy that doubles as the manifest's file inventory, and re-verification of a
//! tree against a manifest. ADR-068 rule 22: what we keep carries a checksum verified
//! on read; corruption is detected and reported, never read as data.
//!
//! The tree walk is iterative with an explicit queue (ADR-068 rule 8, no recursion)
//! and a node-count bound (rule 7/8: nothing grows without a bound).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{SnapshotError, SnapshotResult};
use crate::provider::{Progress, ProgressSink, VerifyReport};

/// Hard bound on files per snapshot; refusing early beats a walk that never ends on a
/// runaway directory.
const MAX_FILES: u64 = 5_000_000;

/// Hard bound on directory depth of one snapshot; a real PostgreSQL layout fits far
/// inside it, and a hand-made `a/b/c/...` chain would otherwise grow the queue without
/// limit.
const MAX_DEPTH: usize = 64;

/// One file's checksum and size, as the manifest records it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FileChecksum {
    /// Lowercase hex SHA-256 of the file's bytes.
    pub sha256: String,
    /// The file's size in bytes, integer.
    pub bytes: u64,
}

/// Computes the lowercase hex SHA-256 and size of one file.
///
/// # Errors
/// `snapshot.io` when the file cannot be read.
pub async fn sha256_file(path: &Path) -> SnapshotResult<FileChecksum> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut bytes: u64 = 0;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = tokio::io::AsyncReadExt::read(&mut file, &mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok(FileChecksum {
        sha256: hex(&hasher.finalize()),
        bytes,
    })
}

/// Lowercase hex of a byte slice.
fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether `text` is a 64-character lowercase hex digest.
pub fn is_hex_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `rel` is a safe relative path (no absolute, no `..`, no empty segments).
pub fn is_safe_relative(rel: &str) -> bool {
    if rel.is_empty()
        || rel.starts_with('/')
        || rel.starts_with('\\')
        || rel.contains('\0')
        || rel.ends_with('/')
    {
        return false;
    }
    rel.split(['/', '\\']).all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment != ".partial"
            && !segment.contains('\0')
    })
}

/// A per-file record collected while copying: relative path and its checksum.
pub type Inventory = BTreeMap<String, FileChecksum>;

/// Copies a whole directory tree from `source` to `dest`, hashing every file on the
/// way, preserving layout and reporting progress. Symlinks are rejected, not followed
/// (a snapshot must never escape its own tree); empty directories are reproduced.
///
/// # Errors
/// `snapshot.io` when a read or write fails; the partially written `dest` is left for
/// the caller to clean (it is staging, never user data).
pub async fn copy_tree_checked(
    source: &Path,
    dest: &Path,
    progress: &dyn ProgressSink,
) -> SnapshotResult<Inventory> {
    if tokio::fs::symlink_metadata(source)
        .await
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(true)
    {
        return Err(SnapshotError::io(format!(
            "the snapshot source {} does not exist or is a symlink",
            source.display()
        )));
    }
    tokio::fs::create_dir_all(dest).await?;
    let mut inventory = Inventory::new();
    let mut queue: std::collections::VecDeque<(PathBuf, PathBuf, usize)> =
        std::collections::VecDeque::new();
    queue.push_back((source.to_path_buf(), dest.to_path_buf(), 0));
    let mut bytes_done: u64 = 0;
    let mut visited: u64 = 0;
    while let Some((from, to, depth)) = queue.pop_front() {
        visited += 1;
        if visited > MAX_FILES {
            return Err(SnapshotError::io(
                "the source tree exceeds the snapshot file bound",
            ));
        }
        if depth > MAX_DEPTH {
            return Err(SnapshotError::io(
                "the source tree exceeds the snapshot depth bound",
            ));
        }
        let meta = tokio::fs::symlink_metadata(&from)
            .await
            .map_err(|e| SnapshotError::io(format!("cannot stat {}: {e}", from.display())))?;
        if meta.file_type().is_symlink() {
            return Err(SnapshotError::io(format!(
                "refusing to follow symlink {} out of the snapshot",
                from.display()
            )));
        }
        if meta.is_dir() {
            tokio::fs::create_dir_all(&to)
                .await
                .map_err(|e| SnapshotError::io(format!("cannot create {}: {e}", to.display())))?;
            let mut entries = tokio::fs::read_dir(&from)
                .await
                .map_err(|e| SnapshotError::io(format!("cannot list {}: {e}", from.display())))?;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| SnapshotError::io(format!("cannot list {}: {e}", from.display())))?
            {
                queue.push_back((entry.path(), to.join(entry.file_name()), depth + 1));
            }
            continue;
        }
        let checksum = sha256_file(&from).await?;
        tokio::fs::copy(&from, &to).await.map_err(|e| {
            SnapshotError::io(format!(
                "cannot copy {} to {}: {e}",
                from.display(),
                to.display()
            ))
        })?;
        let rel = to
            .strip_prefix(dest)
            .map_err(|_| {
                SnapshotError::io(format!(
                    "copied file {} escaped the staging tree",
                    to.display()
                ))
            })?
            .to_string_lossy()
            .replace('\\', "/");
        bytes_done = bytes_done.saturating_add(checksum.bytes);
        inventory.insert(rel, checksum);
        progress.on(Progress::FileDone { bytes_done });
    }
    Ok(inventory)
}

/// Copies exactly the files named by `inventory` (relative to `snapshot`) into `dest`
/// (same relative paths), re-hashing each file and comparing against the recorded
/// checksum before the copy. Used by restores so copied data is verified as it lands.
///
/// # Errors
/// `snapshot.verify_failed` when a recorded file is missing or a hash differs.
pub async fn copy_manifest_files(
    snapshot: &Path,
    inventory: &Inventory,
    dest: &Path,
    prefix: &str,
    progress: &dyn ProgressSink,
) -> SnapshotResult<Inventory> {
    tokio::fs::create_dir_all(dest).await?;
    let mut bytes_done: u64 = 0;
    for (rel, recorded) in inventory {
        if !is_safe_relative(rel) || !rel.starts_with(prefix) {
            return Err(SnapshotError::VerifyFailed {
                reason: format!("manifest names a path outside the snapshot scope: {rel}"),
            });
        }
        let from = snapshot.join(rel);
        let to = dest.join(rel.strip_prefix(prefix).unwrap_or(rel));
        if let Some(parent) = to.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let actual = sha256_file(&from).await.map_err(|e| match e {
            SnapshotError::Io { source } => SnapshotError::VerifyFailed {
                reason: format!("cannot read {rel}: {source}"),
            },
            other => other,
        })?;
        if actual != *recorded {
            return Err(SnapshotError::VerifyFailed {
                reason: format!(
                    "content of {rel} changed: expected sha256 {} ({} bytes), found {} ({} bytes)",
                    recorded.sha256, recorded.bytes, actual.sha256, actual.bytes
                ),
            });
        }
        tokio::fs::copy(&from, &to).await.map_err(|e| {
            SnapshotError::io(format!(
                "cannot copy {} to {}: {e}",
                from.display(),
                to.display()
            ))
        })?;
        bytes_done = bytes_done.saturating_add(recorded.bytes);
        progress.on(Progress::FileDone { bytes_done });
    }
    Ok(inventory.clone())
}

/// Re-checks every file of `inventory` under `dir` against its recorded checksum and
/// refuses unexpected extra files.
///
/// # Errors
/// `snapshot.verify_failed` naming the offending file.
pub async fn verify_tree(dir: &Path, inventory: &Inventory) -> SnapshotResult<VerifyReport> {
    let mut files_checked: u64 = 0;
    let mut bytes_checked: u64 = 0;
    for (rel, recorded) in inventory {
        let path = dir.join(rel);
        let actual = sha256_file(&path).await.map_err(|e| match e {
            SnapshotError::Io { source } => SnapshotError::VerifyFailed {
                reason: format!("cannot read {rel}: {source}"),
            },
            other => other,
        })?;
        if actual.sha256 != recorded.sha256 || actual.bytes != recorded.bytes {
            return Err(SnapshotError::VerifyFailed {
                reason: format!(
                    "byte(s) of {rel} changed since the manifest was written: expected sha256 {} \
                     ({} bytes), found sha256 {} ({} bytes)",
                    recorded.sha256, recorded.bytes, actual.sha256, actual.bytes
                ),
            });
        }
        files_checked += 1;
        bytes_checked += recorded.bytes;
    }
    // Extra files are corruption too: the snapshot must contain exactly the manifest.
    let mut extras = String::new();
    let mut walked: u64 = 0;
    let prefix_len = dir.to_string_lossy().len().saturating_add(1);
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> = std::collections::VecDeque::new();
    queue.push_back((dir.to_path_buf(), 0));
    while let Some((current, depth)) = queue.pop_front() {
        walked += 1;
        if walked > MAX_FILES || depth > MAX_DEPTH {
            return Err(SnapshotError::io(
                "the snapshot tree exceeds the verification walk bounds",
            ));
        }
        let mut entries = tokio::fs::read_dir(&current).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                queue.push_back((path, depth + 1));
                continue;
            }
            let rel = path
                .to_string_lossy()
                .get(prefix_len..)
                .unwrap_or("")
                .replace('\\', "/");
            if !inventory.contains_key(&rel)
                && rel != "snapshot.toml"
                && !rel.contains(".invariants-failed-")
            {
                extras.push_str(&rel);
                extras.push(' ');
                if extras.len() > 400 {
                    extras.push_str("...");
                    break;
                }
            }
        }
    }
    if !extras.is_empty() {
        return Err(SnapshotError::VerifyFailed {
            reason: format!("the snapshot holds files the manifest does not name: {extras}"),
        });
    }
    Ok(VerifyReport {
        files_checked,
        bytes_checked,
    })
}
