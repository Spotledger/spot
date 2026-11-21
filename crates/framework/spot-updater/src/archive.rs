//! Reading the update archive safely: path safety, per-file hashes, and the bomb guards.
//!
//! The payload is a ZIP whose entries the signed manifest lists with their size and SHA-256.
//! Before anything is written to the staging directory, [`verify_archive`] walks every entry:
//!
//! 1. the entry's name must pass the same path-safety rule `spot-bundle` uses (a plain relative
//!    path: no `..`, no leading `/`, no backslash, no colon, no NUL), so nothing can escape the
//!    staging directory;
//! 2. the entry must be listed in the manifest (an unlisted file is refused), and every listed
//!    file must be present;
//! 3. the entry may not inflate beyond the size the manifest declares, beyond the per-file
//!    limit, beyond the total limit, or beyond a compression ratio, so a zip bomb dies while it
//!    is read rather than after it has filled the disk;
//! 4. the entry's bytes must hash to its manifest digest.
//!
//! The rule in point 1 is copied from `spot-bundle` (`src/paths.rs`): the rule, not the code.

use std::collections::BTreeMap;
use std::io::{Cursor, Read as _};

use zip::ZipArchive;

use crate::digest::Digest;
use crate::error::UpdaterError;
use crate::manifest::UpdateManifest;
use crate::verify::verify_file;

/// The bounds [`verify_archive`] enforces while reading the update payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveLimits {
    /// The largest number of entries (files and directories) the archive may hold.
    pub max_entries: u32,
    /// The largest total inflated size of every entry together, in bytes.
    pub max_total_bytes: u64,
    /// The largest inflated size of one entry, in bytes.
    pub max_file_bytes: u64,
    /// The largest ratio of an entry's inflated size to its compressed size.
    pub max_compression_ratio: u64,
}

impl ArchiveLimits {
    /// The defaults: 4096 entries, 256 MiB total, 128 MiB one file, ratio 200.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_entries: 4096,
            max_total_bytes: 256 * 1024 * 1024,
            max_file_bytes: 128 * 1024 * 1024,
            max_compression_ratio: 200,
        }
    }
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// One verified file, ready to be written under the version directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedFile {
    /// The file's path, relative to the version directory, already path-safe with `/`.
    pub path: String,
    /// The verified bytes.
    pub bytes: Vec<u8>,
}

/// Validates `archive` against the signed `manifest` within `limits`, returning every file's
/// verified bytes. Nothing is written; the engine writes the result to staging.
///
/// # Errors
/// [`UpdaterError::Archive`] when the bytes are not a readable ZIP or an entry is malformed,
/// [`UpdaterError::ArchiveEscape`] for a name that would escape staging,
/// [`UpdaterError::ArchiveUnlisted`] or [`UpdaterError::ArchiveMissing`] for a set mismatch,
/// [`UpdaterError::ArchiveBomb`] for an entry over a size or ratio limit, and the part-1
/// digest/size codes when an entry's bytes do not match the manifest.
pub fn verify_archive(
    manifest: &UpdateManifest,
    archive: &[u8],
    limits: &ArchiveLimits,
) -> Result<Vec<StagedFile>, UpdaterError> {
    let mut declared: BTreeMap<String, (u64, Digest)> = BTreeMap::new();
    for file in &manifest.files {
        let name = normalise(&file.path)?;
        if declared
            .keys()
            .any(|other| other.eq_ignore_ascii_case(&name))
        {
            return Err(UpdaterError::Archive {
                detail: format!("duplicate manifest path `{name}`"),
            });
        }
        declared.insert(name, (file.size, file.sha256));
    }

    let mut zip = ZipArchive::new(Cursor::new(archive)).map_err(|error| UpdaterError::Archive {
        detail: error.to_string(),
    })?;
    if u64::try_from(zip.len()).unwrap_or(u64::MAX) > u64::from(limits.max_entries) {
        return Err(UpdaterError::ArchiveBomb {
            entry: "<archive>".to_owned(),
            detail: format!(
                "{} entries is over the limit of {}",
                zip.len(),
                limits.max_entries
            ),
        });
    }

    let mut out: Vec<StagedFile> = Vec::new();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    let mut total: u64 = 0;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|error| UpdaterError::Archive {
            detail: error.to_string(),
        })?;
        if entry.is_symlink() {
            return Err(UpdaterError::Archive {
                detail: format!(
                    "`{}` is a symlink; the payload carries plain files",
                    display_name(&entry)
                ),
            });
        }
        if entry.encrypted() {
            return Err(UpdaterError::Archive {
                detail: format!(
                    "`{}` is encrypted; the payload must not be",
                    display_name(&entry)
                ),
            });
        }
        let raw = std::str::from_utf8(entry.name_raw()).map_err(|_| UpdaterError::Archive {
            detail: "an entry name is not valid UTF-8".to_owned(),
        })?;
        let name = normalise(raw)?;
        if entry.is_dir() {
            continue;
        }
        if seen.keys().any(|other| other.eq_ignore_ascii_case(&name)) {
            return Err(UpdaterError::Archive {
                detail: format!("duplicate archive path `{name}`"),
            });
        }
        let Some((size, digest)) = declared.get(&name).copied() else {
            return Err(UpdaterError::ArchiveUnlisted { entry: name });
        };
        let remaining = ArchiveLimits {
            max_file_bytes: limits
                .max_file_bytes
                .min(limits.max_total_bytes.saturating_sub(total)),
            ..*limits
        };
        let bytes = read_bounded(&mut entry, size, &name, &remaining)?;
        verify_file(
            &crate::manifest::UpdateFile {
                path: name.clone(),
                size,
                sha256: digest,
            },
            bytes.len() as u64,
            Digest::of(&bytes),
        )?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| UpdaterError::ArchiveBomb {
                entry: name.clone(),
                detail: "the total size overflowed".to_owned(),
            })?;
        if total > limits.max_total_bytes {
            return Err(UpdaterError::ArchiveBomb {
                entry: name,
                detail: format!("the total inflates past {} bytes", limits.max_total_bytes),
            });
        }
        seen.insert(name.clone(), ());
        out.push(StagedFile { path: name, bytes });
    }

    for name in declared.keys() {
        if !seen.contains_key(name) {
            return Err(UpdaterError::ArchiveMissing {
                entry: name.clone(),
            });
        }
    }
    Ok(out)
}

/// The entry name as zip decodes it (UTF-8, else CP437), for error messages only. zip 9 decodes
/// lazily and can fail; the raw bytes, lossily decoded, stand in then.
fn display_name<R: std::io::Read + ?Sized>(entry: &zip::read::ZipFile<'_, R>) -> String {
    entry.name().map_or_else(
        |_| String::from_utf8_lossy(entry.name_raw()).into_owned(),
        std::borrow::Cow::into_owned,
    )
}

/// Reads one entry, refusing more than `declared` bytes, more than the per-file limit, or a
/// compression ratio over the limit. The ratio is measured from the bytes actually read, never
/// taken from the archive's declarations.
fn read_bounded<R: std::io::Read + ?Sized>(
    entry: &mut zip::read::ZipFile<'_, R>,
    declared: u64,
    name: &str,
    limits: &ArchiveLimits,
) -> Result<Vec<u8>, UpdaterError> {
    let budget = declared.min(limits.max_file_bytes);
    let mut out: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 65_536];
    loop {
        let read = entry
            .read(&mut chunk)
            .map_err(|error| UpdaterError::Archive {
                detail: format!("{name}: {error}"),
            })?;
        if read == 0 {
            break;
        }
        out.extend_from_slice(chunk.get(..read).unwrap_or(&[]));
        let inflated = out.len() as u64;
        if inflated > budget {
            return Err(UpdaterError::ArchiveBomb {
                entry: name.to_owned(),
                detail: format!("it inflates past the {budget} bytes it declares"),
            });
        }
        let compressed = entry.compressed_size();
        if inflated > compressed.saturating_mul(limits.max_compression_ratio) {
            return Err(UpdaterError::ArchiveBomb {
                entry: name.to_owned(),
                detail: format!(
                    "it inflates {inflated} bytes from {compressed}, over the ratio of {}",
                    limits.max_compression_ratio
                ),
            });
        }
    }
    Ok(out)
}

/// Validates an entry name and normalises it: `.` segments and `//` collapse, everything that
/// could escape the version directory is an error. The rule is `spot-bundle`'s, copied.
///
/// # Errors
/// [`UpdaterError::ArchiveEscape`] for a name that is absolute, holds `..`, a backslash, a
/// colon or a NUL, is not UTF-8, or is empty after normalisation.
pub fn normalise(raw: &str) -> Result<String, UpdaterError> {
    if raw.contains('\\') {
        return Err(UpdaterError::ArchiveEscape {
            entry: raw.to_owned(),
        });
    }
    if raw.contains('\0') {
        return Err(UpdaterError::ArchiveEscape {
            entry: raw.to_owned(),
        });
    }
    if raw.starts_with('/') {
        return Err(UpdaterError::ArchiveEscape {
            entry: raw.to_owned(),
        });
    }
    // A colon looks like a Windows drive or an alternate data stream, so it is never a plain
    // relative path.
    if raw.contains(':') {
        return Err(UpdaterError::ArchiveEscape {
            entry: raw.to_owned(),
        });
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in raw.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                return Err(UpdaterError::ArchiveEscape {
                    entry: raw.to_owned(),
                });
            }
            other => segments.push(other),
        }
    }
    let name = segments.join("/");
    if name.is_empty() {
        return Err(UpdaterError::ArchiveEscape {
            entry: raw.to_owned(),
        });
    }
    Ok(name)
}
