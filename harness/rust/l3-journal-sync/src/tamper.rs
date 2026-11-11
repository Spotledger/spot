//! Byte-level corruption helpers for the later journal/sync tasks' recovery tests.
//!
//! Every helper works directly on the filesystem (the layout [`spot_journal_store::FolderStore`]
//! uses), deliberately bypassing [`spot_journal_store::JournalStore`]'s own rules (immutability,
//! the delete guard): that is the point of a tamper helper, which exists to simulate damage a
//! well-behaved store would never do to itself.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Flips every bit of the byte at `offset` in the file at `path` (XOR `0xFF`), so the new byte
/// is guaranteed to differ from the old one no matter what it was, and every other byte is
/// untouched.
///
/// # Errors
/// Any [`std::io::Error`] opening, seeking, reading or writing the file, including `offset`
/// being at or past the end of the file.
pub fn flip_byte(path: &Path, offset: u64) -> std::io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut byte = [0u8; 1];
    let read = file.read(&mut byte)?;
    if read == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!(
                "offset {offset} is at or past the end of {}",
                path.display()
            ),
        ));
    }
    byte[0] ^= 0xFF;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&byte)?;
    file.sync_all()
}

/// Truncates the file at `path` to `new_len` bytes, simulating a crash mid-write that left a
/// short file.
///
/// # Errors
/// Any [`std::io::Error`] opening or truncating the file.
pub fn truncate(path: &Path, new_len: u64) -> std::io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(new_len)?;
    file.sync_all()
}

/// Removes the object `key` from a folder store rooted at `root`, bypassing
/// [`spot_journal_store::JournalStore::delete`]'s guard: this can remove a segment or a
/// snapshot, which the guarded trait method refuses, because the point is to simulate the
/// provider losing an object the journal trusted to be permanent.
///
/// A missing object is not an error (removing something already gone is a no-op): `Ok(false)`.
///
/// # Errors
/// Any [`std::io::Error`] other than the object already being absent.
pub fn delete_object(root: &Path, key: &str) -> std::io::Result<bool> {
    let path = root.join(key);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn scratch_file(label: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "l3_journal_sync_tamper_{label}_{}",
            std::process::id()
        ));
        std::fs::write(&path, bytes).expect("write the scratch file");
        path
    }

    #[test]
    fn flip_byte_changes_exactly_one_byte() {
        let original: Vec<u8> = (0u8..32).collect();
        let path = scratch_file("flip", &original);
        flip_byte(&path, 10).expect("flip a byte");
        let after = std::fs::read(&path).expect("read back");
        assert_eq!(after.len(), original.len(), "the length must not change");
        let differences: Vec<usize> = (0..original.len())
            .filter(|&i| original[i] != after[i])
            .collect();
        assert_eq!(differences, vec![10], "exactly one byte must differ");
        assert_eq!(after[10], original[10] ^ 0xFF);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn flip_byte_past_the_end_is_an_error() {
        let path = scratch_file("flip_eof", b"short");
        let result = flip_byte(&path, 100);
        assert!(result.is_err(), "an offset past the end must fail");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn truncate_shortens_the_file_without_touching_the_kept_bytes() {
        let path = scratch_file("truncate", b"0123456789");
        truncate(&path, 4).expect("truncate");
        let after = std::fs::read(&path).expect("read back");
        assert_eq!(after, b"0123");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_object_removes_the_file_under_the_root() {
        let root = std::env::temp_dir().join(format!(
            "l3_journal_sync_tamper_delete_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("segments")).expect("mkdir");
        let key = "segments/00000000000000000001.segment";
        std::fs::write(root.join(key), b"sealed").expect("write the object");
        assert!(delete_object(&root, key).expect("delete"));
        assert!(!root.join(key).exists());
        // A second delete of the same key is a no-op, not an error.
        assert!(!delete_object(&root, key).expect("second delete"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
