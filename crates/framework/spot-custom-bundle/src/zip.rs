//! The ZIP container: a deterministic writer and a bounded, path-safe reader.
//!
//! The writer follows `spot-bundle`'s discipline: entries in byte-wise sorted name order,
//! the fixed 1980 timestamp, DEFLATE, no extra fields or permissions. The reader is this
//! crate's own: it enforces the limits against the bytes actually inflated (a zip bomb dies
//! early), rejects a name that could escape the bundle, refuses symlinks and encrypted
//! entries, and reports duplicates the central directory carries.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek, Write as _};

use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::error::{CustomBundleError, codes};
use crate::limits::Limits;
use crate::paths::{PathError, SafeName, case_key, normalise};

/// Writes `entries` as a deterministic ZIP.
pub(crate) fn write_zip(entries: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, CustomBundleError> {
    let buffer = std::io::Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(buffer);
    for (name, bytes) in entries {
        zip.start_file(
            name.clone(),
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .map_err(|err| {
            CustomBundleError::new(
                codes::BAD_DEFLATE,
                format!("the bundle could not be packed: {err}"),
            )
        })?;
        zip.write_all(bytes).map_err(|err| {
            CustomBundleError::new(
                codes::BAD_DEFLATE,
                format!("the bundle could not be written: {err}"),
            )
        })?;
    }
    let buffer = zip.finish().map_err(|err| {
        CustomBundleError::new(
            codes::BAD_DEFLATE,
            format!("the bundle could not be closed: {err}"),
        )
    })?;
    Ok(buffer.into_inner())
}

/// Reads every plain file entry within `limits`, reporting **every** problem it can find.
///
/// The returned map keys are normalised names. Directory entries are allowed and ignored.
pub(crate) fn read_zip(
    bytes: &[u8],
    limits: &Limits,
) -> Result<BTreeMap<String, Vec<u8>>, Vec<CustomBundleError>> {
    if bytes.len() as u64 > limits.max_compressed_bytes {
        return Err(vec![CustomBundleError::new(
            codes::TOO_LARGE,
            format!(
                "the bundle is {} bytes; the limit is {}",
                bytes.len(),
                limits.max_compressed_bytes
            ),
        )]);
    }
    let mut zip =
        ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|err| vec![open_fault(err)])?;
    let count = zip.len() as u32;
    let mut errors = Vec::new();
    if count > limits.max_entries {
        errors.push(CustomBundleError::new(
            codes::TOO_MANY_ENTRIES,
            format!(
                "the bundle has {count} entries; the limit is {}",
                limits.max_entries
            ),
        ));
    }
    // The zip crate drops a repeated name from its index; the central directory lists every
    // record, so duplicates are found there.
    if let Some(names) = central_directory_names(bytes) {
        errors.extend(duplicate_faults(&names));
    }

    let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut total: u64 = 0;
    let n = count.min(limits.max_entries);
    for index in 0..n as usize {
        let meta = match entry_meta(&mut zip, index) {
            Ok(meta) => meta,
            Err(err) => {
                errors.push(zip_fault(index, err));
                continue;
            }
        };
        let name = match normalise(&meta.raw_name) {
            Ok(name) => name,
            Err(fault) => {
                errors.push(path_fault(&meta.raw_name, &fault));
                continue;
            }
        };
        if meta.encrypted {
            errors.push(CustomBundleError::about(
                codes::BAD_DEFLATE,
                name.as_str().to_owned(),
                "the entry is encrypted; bundles carry plain entries only",
            ));
            continue;
        }
        if meta.is_symlink {
            errors.push(CustomBundleError::about(
                codes::BAD_SYMLINK,
                name.as_str().to_owned(),
                "a bundle carries plain files and directories, not symlinks",
            ));
            continue;
        }
        if meta.is_dir {
            continue;
        }
        let key = case_key(&name);
        if let Some(first) = seen.get(&key) {
            let why = if first == name.as_str() {
                "the same name appears twice"
            } else {
                "a second name that differs only by case (they collide on Windows)"
            };
            errors.push(CustomBundleError::about(
                codes::DUPLICATE_ENTRY,
                name.as_str().to_owned(),
                why,
            ));
            continue;
        }
        let budget = limits.max_file_bytes.min(limits.max_total_bytes - total);
        let content = match read_entry(&mut zip, index, budget) {
            Ok(content) => content,
            Err(ReadFault::OverBudget) => {
                errors.push(CustomBundleError::about(
                    codes::TOO_LARGE,
                    name.as_str().to_owned(),
                    format!(
                        "the file inflates past {budget} bytes (the limit for one file, or what \
                         is left of the total); refused before its full size was allocated"
                    ),
                ));
                continue;
            }
            Err(ReadFault::Zip(err)) => {
                errors.push(zip_fault(index, err));
                continue;
            }
        };
        total += content.len() as u64;
        seen.insert(key, name.as_str().to_owned());
        entries.insert(name.as_str().to_owned(), content);
    }

    if errors.is_empty() {
        Ok(entries)
    } else {
        Err(errors)
    }
}

/// What [`entry_meta`] reports about one entry.
struct EntryMeta {
    raw_name: Vec<u8>,
    is_dir: bool,
    is_symlink: bool,
    encrypted: bool,
}

/// Why one entry could not be inflated.
enum ReadFault {
    /// The entry inflates past its budget: a bomb, cut off early.
    OverBudget,
    /// The zip layer failed: bad deflate, a CRC mismatch, encryption, a lost entry.
    Zip(ZipError),
}

/// Reads one entry's metadata from the archive.
fn entry_meta<R: std::io::Read + Seek>(
    zip: &mut ZipArchive<R>,
    index: usize,
) -> Result<EntryMeta, ZipError> {
    let file = zip.by_index_raw(index)?;
    Ok(EntryMeta {
        raw_name: file.name_raw().to_vec(),
        is_dir: file.is_dir(),
        is_symlink: file.is_symlink(),
        encrypted: file.encrypted(),
    })
}

/// Inflates one entry in full, verifying its CRC, but never producing more than `budget`.
fn read_entry<R: std::io::Read + Seek>(
    zip: &mut ZipArchive<R>,
    index: usize,
    budget: u64,
) -> Result<Vec<u8>, ReadFault> {
    let mut file = zip.by_index(index).map_err(ReadFault::Zip)?;
    let mut out = Vec::new();
    let mut chunk = [0_u8; 65_536];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                out.extend_from_slice(&chunk[..n]);
                if out.len() as u64 > budget {
                    return Err(ReadFault::OverBudget);
                }
            }
            Err(err) => return Err(ReadFault::Zip(ZipError::from(err))),
        }
    }
}

/// Turns a failure to open the archive at all into a problem.
fn open_fault(err: ZipError) -> CustomBundleError {
    match err {
        ZipError::InvalidArchive(why) => CustomBundleError::new(
            codes::BAD_ARCHIVE,
            format!("the archive is malformed: {why}"),
        ),
        ZipError::UnsupportedArchive(why) => CustomBundleError::new(
            codes::BAD_ARCHIVE,
            format!("the archive uses a feature this bundle format does not support: {why}"),
        ),
        other => CustomBundleError::new(
            codes::BAD_ARCHIVE,
            format!("the archive could not be read: {other}"),
        ),
    }
}

/// Turns a zip failure on one entry into a problem, naming the entry's index when its name is
/// not known.
fn zip_fault(index: usize, err: ZipError) -> CustomBundleError {
    match err {
        ZipError::InvalidArchive(why) => CustomBundleError::new(
            codes::BAD_ARCHIVE,
            format!("entry {index} of the archive is malformed: {why}"),
        ),
        ZipError::UnsupportedArchive(why) => CustomBundleError::new(
            codes::BAD_ARCHIVE,
            format!("entry {index} uses a feature this bundle format does not support: {why}"),
        ),
        ZipError::FileNotFound => CustomBundleError::new(
            codes::BAD_ARCHIVE,
            format!("the archive lost entry {index} between listing and reading"),
        ),
        other => CustomBundleError::new(
            codes::BAD_DEFLATE,
            format!("entry {index} could not be read: {other}"),
        ),
    }
}

/// Turns a path error into `custom_bundle.illegal_path`, naming the raw entry.
fn path_fault(raw: &[u8], fault: &PathError) -> CustomBundleError {
    let shown = String::from_utf8_lossy(raw).into_owned();
    match fault {
        PathError::Empty => CustomBundleError::about(
            codes::ILLEGAL_PATH,
            shown,
            "the name is empty after normalisation (it held only `.` and `/`)",
        ),
        PathError::NotRelative(why) => CustomBundleError::about(
            codes::ILLEGAL_PATH,
            shown,
            format!("the entry name is not a plain relative path: {why}"),
        ),
        PathError::NotUtf8 => {
            CustomBundleError::about(codes::ILLEGAL_PATH, shown, "the name is not valid UTF-8")
        }
    }
}

/// Walks the archive's central directory and returns every entry's raw name, in directory
/// order — duplicates included.
///
/// `zip::ZipArchive` keys entries by name and silently drops a repeat, so the duplicate check
/// cannot run on what it exposes; the central directory is the format's authoritative listing
/// and lists every record. Classic EOCD only: a bundle is capped far below the 4 GiB /
/// 65 535-entry boundaries where zip64 would be needed.
pub(crate) fn central_directory_names(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    const EOCD: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
    const CD_HEADER: [u8; 4] = [0x50, 0x4B, 0x01, 0x02];
    if bytes.len() < 22 {
        return None;
    }
    let mut eocd = None;
    for i in (0..=bytes.len() - 22).rev() {
        if bytes[i..i + 4] == EOCD {
            eocd = Some(i);
            break;
        }
    }
    let eocd = eocd?;
    let read16 = |off: usize| u16::from_le_bytes([bytes[off], bytes[off + 1]]);
    let read32 = |off: usize| {
        u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
    };
    let count = read16(eocd + 10) as usize;
    let cd_offset = read32(eocd + 16) as usize;
    let mut names = Vec::with_capacity(count);
    let mut cursor = cd_offset;
    for _ in 0..count {
        if cursor + 46 > bytes.len() || bytes[cursor..cursor + 4] != CD_HEADER {
            return None;
        }
        let name_len = read16(cursor + 28) as usize;
        let extra_len = read16(cursor + 30) as usize;
        let comment_len = read16(cursor + 32) as usize;
        let name_at = cursor + 46;
        if name_at + name_len > bytes.len() {
            return None;
        }
        names.push(bytes[name_at..name_at + name_len].to_vec());
        cursor = name_at + name_len + extra_len + comment_len;
    }
    Some(names)
}

/// Detects duplicate entry names from the central directory listing: exact repeats and names
/// differing only by ASCII case.
pub(crate) fn duplicate_faults(names: &[Vec<u8>]) -> Vec<CustomBundleError> {
    let mut errors = Vec::new();
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for raw in names {
        let Ok(name) = normalise(raw) else {
            // Path problems are reported by the reader; this pass only hunts duplicates.
            continue;
        };
        let key = case_key(&name);
        if let Some(first) = seen.get(&key) {
            let why = if first == name.as_str() {
                "the same name appears twice in the bundle"
            } else {
                "a second name that differs only by case (they collide on Windows)"
            };
            errors.push(CustomBundleError::about(
                codes::DUPLICATE_ENTRY,
                name.as_str().to_owned(),
                why,
            ));
            continue;
        }
        seen.insert(key, name.as_str().to_owned());
    }
    errors
}

/// A normalised name that is known to start with `sections/<kind>/`.
pub(crate) fn split_section(name: &SafeName) -> Option<(String, String)> {
    let rest = name.as_str().strip_prefix("sections/")?;
    let (kind, file) = rest.split_once('/')?;
    if kind.is_empty() || file.is_empty() {
        return None;
    }
    Some((kind.to_owned(), file.to_owned()))
}
