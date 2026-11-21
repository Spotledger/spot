//! Windows adapter for the two things `std` cannot do: a durable directory flush (no
//! unsafe code needed) and the free space of a volume (one small `unsafe` call).
//!
//! This is the only module of the crate that is allowed `unsafe` (the crate sets
//! `unsafe_code = "deny"` in `Cargo.toml`, the same exception `spot-pg-supervisor` takes).

#![allow(unsafe_code)]

use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

/// `FILE_FLAG_BACKUP_SEMANTICS`: required by `CreateFileW` to open a directory handle.
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// Flushes the directory `path` to durable storage: a handle opened with backup semantics and
/// write access, then `FlushFileBuffers` (`File::sync_all`). A volume that refuses the flush
/// (access denied, or a file system that cannot flush a directory) is an error naming the
/// directory, never a silent success.
pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    let outcome = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .and_then(|directory| directory.sync_all());
    outcome.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("flush directory {}: {error}", path.display()),
        )
    })
}

/// The bytes available to the caller on the volume that holds `path`. A relative path is
/// resolved against the current directory first; a path that does not exist yet is measured
/// through its nearest existing directory. UNC and `\\?\` paths are passed as they are.
pub(super) fn free_space(path: &Path) -> io::Result<u64> {
    let absolute = std::path::absolute(path)?;
    let directory = nearest_directory(&absolute).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no existing directory on the path")
    })?;
    let mut wide: Vec<u16> = directory.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path contains a NUL character",
        ));
    }
    wide.push(0);
    let mut available: u64 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that lives across the call;
    // `available` is a valid, writable `u64`; the two optional out-parameters are null, which
    // the API documents as "not wanted".
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(available)
}

/// `path` itself when it is a directory, otherwise its nearest ancestor that is one.
fn nearest_directory(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|p| p.is_dir()).map(Path::to_path_buf)
}
