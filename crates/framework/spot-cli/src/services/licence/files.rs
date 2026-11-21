//! The files of the licence client (WP-CTL-04.T3): the licence, the clock-guard observation, the
//! last renewal attempt and the trusted issuer keys.
//!
//! None of them is a secret, but all are integrity-critical, so each is checked for its mode (not
//! writable by group or others) and every write is atomic: the new content goes to a temporary
//! file created with `create_new`, is fsynced, renamed over the old one, and the directory is
//! synced. A crash between the temporary file and the rename leaves the old file whole and
//! readable; a leftover temporary file is replaced by the next write.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use spot_license::{Observation, Store, TrustedKey, TrustedKeys};

/// The largest licence, key or state file read (a licence is a few hundred bytes).
pub const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The temporary sibling of `path`: same directory, `.new` appended to the file name.
#[must_use]
pub fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    name.push(".new");
    path.with_file_name(name)
}

/// Step one of an atomic write: the content is on disk under the temporary name, fsynced.
/// Exposed so a test can stop here ("killed between the temp file and the rename").
///
/// # Errors
/// A fixed sentence.
pub fn write_temp(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let tmp = temp_path(path);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|_| "the directory cannot be created".to_owned())?;
    }
    // A temporary file left by a crash is not trusted: replace it.
    let _ = fs::remove_file(&tmp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|_| "the file cannot be written".to_owned())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| {
            let _ = fs::remove_file(&tmp);
            "the file cannot be written".to_owned()
        })?;
    Ok(tmp)
}

/// Step two: rename the temporary file over `path` and sync the directory.
///
/// # Errors
/// A fixed sentence.
pub fn commit_temp(tmp: &Path, path: &Path) -> Result<(), String> {
    fs::rename(tmp, path).map_err(|_| {
        let _ = fs::remove_file(tmp);
        "the file cannot be replaced".to_owned()
    })?;
    #[cfg(unix)]
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Replaces `path` with `bytes` atomically.
///
/// # Errors
/// A fixed sentence.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = write_temp(path, bytes)?;
    commit_temp(&tmp, path)
}

/// Refuses a file that group or others can write.
///
/// # Errors
/// A sentence naming the file (a path is configuration; its content is never quoted).
pub fn check_not_writable_by_others(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path)
            && meta.permissions().mode() & 0o022 != 0
        {
            return Err(format!(
                "{} must not be writable by group or others",
                path.display()
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Reads a small text file: `Ok(None)` when it does not exist.
///
/// # Errors
/// A sentence naming the file when it is unreadable, too large, or writable by others.
pub fn read_optional(path: &Path) -> Result<Option<String>, String> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(format!("{} cannot be read", path.display())),
    };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(format!("{} is not a small regular file", path.display()));
    }
    check_not_writable_by_others(path)?;
    fs::read_to_string(path)
        .map(Some)
        .map_err(|_| format!("{} cannot be read", path.display()))
}

/// The clock guard's observation in a one-line file: `<high water mark> <monotonic seconds>`.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    /// A store over `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Store for FileStore {
    fn load(&self) -> Result<Option<Observation>, String> {
        let Some(text) = read_optional(&self.path)? else {
            return Ok(None);
        };
        let mut parts = text.split_whitespace();
        let mark = parts.next().and_then(|p| p.parse::<i64>().ok());
        let mono = parts.next().and_then(|p| p.parse::<u64>().ok());
        match (mark, mono) {
            (Some(high_water_mark), Some(monotonic_seconds)) => Ok(Some(Observation {
                high_water_mark,
                monotonic_seconds,
            })),
            _ => Err("the clock-guard state file is malformed".to_owned()),
        }
    }

    fn save(&mut self, observation: &Observation) -> Result<(), String> {
        let text = format!(
            "{} {}\n",
            observation.high_water_mark, observation.monotonic_seconds
        );
        atomic_write(&self.path, text.as_bytes())
    }
}

/// The last renewal attempt: when and the stable code of its result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// Unix seconds.
    pub at: i64,
    /// The result code (`ok`, or a stable code).
    pub code: String,
}

/// Reads the attempt file; `None` when none was ever written or it is malformed.
#[must_use]
pub fn read_attempt(path: &Path) -> Option<Attempt> {
    let text = read_optional(path).ok()??;
    let (at, code) = text.trim().split_once(' ')?;
    Some(Attempt {
        at: at.parse().ok()?,
        code: code.to_owned(),
    })
}

/// Writes the attempt file.
///
/// # Errors
/// A fixed sentence.
pub fn write_attempt(path: &Path, attempt: &Attempt) -> Result<(), String> {
    atomic_write(
        path,
        format!("{} {}\n", attempt.at, attempt.code).as_bytes(),
    )
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .chunks(2)
        .map(|pair| Some(hex_value(pair[0])? << 4 | hex_value(pair[1])?))
        .collect()
}

/// Parses the trusted issuer keys. One key per line, `#` starts a comment:
///
/// ```text
/// <key id> <64 hex digits of the Ed25519 public key> <valid_from unix seconds> <valid_to | ->
/// ```
///
/// # Errors
/// A sentence naming the line number; never the key.
pub fn parse_trusted_keys(text: &str) -> Result<TrustedKeys, String> {
    let mut keys = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let bad = || {
            format!(
                "trusted keys line {}: expected `<key id> <hex key> <valid_from> <valid_to|->`",
                index + 1
            )
        };
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [key_id, hex, from, to] = parts[..] else {
            return Err(bad());
        };
        let public = decode_hex(hex).ok_or_else(bad)?;
        let valid_from: i64 = from.parse().map_err(|_| bad())?;
        let valid_to = if to == "-" {
            None
        } else {
            Some(to.parse::<i64>().map_err(|_| bad())?)
        };
        let key = TrustedKey::from_bytes(key_id, &public, valid_from, valid_to).map_err(|_| {
            format!(
                "trusted keys line {}: the key is not a valid Ed25519 public key",
                index + 1
            )
        })?;
        keys.push(key);
    }
    if keys.is_empty() {
        return Err("the trusted keys file lists no key".to_owned());
    }
    Ok(TrustedKeys::new(keys))
}
