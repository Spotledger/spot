//! Durable state of the channel: the per-command record (persisted with fsync **before**
//! a command executes) and the file-backed replay window.
//!
//! The agent has no PostgreSQL, so replay protection survives a restart through
//! [`FileNonceStore`], a `NonceStore` over a small file: the window is rewritten by
//! write-then-rename (fsynced) when it is opened and swept, and every accepted nonce is
//! appended and fsynced **before** `check_and_insert` answers `true`, so the command can
//! only execute after its nonce is on disk.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use spot_signed_command::{KeyId, NonceFuture, NonceStore, StoreError};

use crate::error::AgentError;
use crate::state::atomic_write;

/// Hard cap on live nonces: at the cap new commands are refused, never evicted.
pub const NONCE_CAP: usize = 100_000;
/// Skew added to an entry's life (the verifier's maximum skew).
pub const NONCE_SKEW: i64 = 60;

/// Where a command is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmdState {
    /// Accepted (verified, nonce on disk), handler not started.
    Pending,
    /// The handler was started; if the agent dies now the outcome is unknown.
    Started,
    /// Finished; the result is stored.
    Done,
}

/// One command's durable record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRecord {
    /// The command id (the contract's identity of a command).
    pub id: String,
    /// The state.
    pub state: CmdState,
    /// The core wire envelope, kept until the command is done (for the restart re-check).
    pub envelope: String,
    /// Result sequence number (0 until done).
    pub seq: u64,
    /// `succeeded`, `failed`, `refused`, `unsupported` or `indeterminate`.
    pub outcome: Option<String>,
    /// The result code.
    pub code: Option<String>,
    /// Redacted output, at most 64 KiB.
    pub output: Option<String>,
    /// The gateway acknowledged the result.
    pub reported: bool,
}

/// Whether `id` is safe as a file name and within the contract's bounds.
#[must_use]
pub fn valid_command_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The directory of command records.
#[derive(Clone, Debug)]
pub struct FileCommandStore {
    dir: PathBuf,
}

impl FileCommandStore {
    /// Opens (creating) the directory.
    ///
    /// # Errors
    /// [`AgentError::StateIo`].
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .map_err(|_| AgentError::StateIo("cannot create the command store".to_owned()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Self { dir })
    }

    fn path(&self, id: &str) -> Result<PathBuf, AgentError> {
        if !valid_command_id(id) {
            return Err(AgentError::GatewayProtocol(
                "the command id is invalid".to_owned(),
            ));
        }
        Ok(self.dir.join(format!("{id}.json")))
    }

    /// The record of `id`, if any.
    ///
    /// # Errors
    /// [`AgentError::StateIo`] when the file exists but cannot be read or parsed.
    pub fn get(&self, id: &str) -> Result<Option<CommandRecord>, AgentError> {
        let path = self.path(id)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|_| AgentError::StateIo("a command record is corrupt".to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(AgentError::StateIo(
                "cannot read a command record".to_owned(),
            )),
        }
    }

    /// Writes the record atomically (temp file, fsync, rename).
    ///
    /// # Errors
    /// [`AgentError::StateIo`].
    pub fn put(&self, record: &CommandRecord) -> Result<(), AgentError> {
        let text = serde_json::to_string(record)
            .map_err(|_| AgentError::StateIo("cannot encode a record".to_owned()))?;
        atomic_write(&self.path(&record.id)?, text.as_bytes())
    }

    /// Every record, oldest result first (pending ones last, by id).
    ///
    /// # Errors
    /// [`AgentError::StateIo`].
    pub fn list(&self) -> Result<Vec<CommandRecord>, AgentError> {
        let mut out = Vec::new();
        let read = std::fs::read_dir(&self.dir)
            .map_err(|_| AgentError::StateIo("cannot list the command store".to_owned()))?;
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json")
                && let Some(record) = self.get(id)?
            {
                out.push(record);
            }
        }
        out.sort_by(|a, b| (a.seq == 0, a.seq, &a.id).cmp(&(b.seq == 0, b.seq, &b.id)));
        Ok(out)
    }

    /// The next result sequence number (persisted, strictly increasing).
    ///
    /// # Errors
    /// [`AgentError::StateIo`].
    pub fn next_seq(&self) -> Result<u64, AgentError> {
        let path = self.dir.join("seq");
        let last: u64 = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0);
        let next = last.saturating_add(1);
        atomic_write(&path, next.to_string().as_bytes())?;
        Ok(next)
    }

    /// The highest sequence number the gateway acknowledged.
    ///
    /// # Errors
    /// [`AgentError::StateIo`].
    pub fn last_reported_seq(&self) -> Result<u64, AgentError> {
        Ok(self
            .list()?
            .iter()
            .filter(|r| r.reported)
            .map(|r| r.seq)
            .max()
            .unwrap_or(0))
    }
}

/// The file-backed replay window.
pub struct FileNonceStore {
    path: PathBuf,
    entries: Mutex<HashMap<(String, [u8; 16]), i64>>,
}

fn hex16(n: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    n.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn parse_hex16(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl FileNonceStore {
    /// Opens the window at `path`, dropping entries past `expires_at + skew` at `now` and
    /// rewriting the file by write-then-rename when anything was dropped. A torn last line
    /// (crash during an append that was never acknowledged) is ignored.
    ///
    /// # Errors
    /// [`AgentError::StateIo`] when the file is unreadable or a complete line is corrupt.
    pub fn open(path: impl Into<PathBuf>, now: i64) -> Result<Self, AgentError> {
        let path = path.into();
        let mut entries = HashMap::new();
        let mut dropped = false;
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let complete = text.ends_with('\n');
                let lines: Vec<&str> = text.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    let mut parts = line.split(' ');
                    let parsed = (|| {
                        let key = parts.next()?;
                        let nonce = parse_hex16(parts.next()?)?;
                        let exp: i64 = parts.next()?.parse().ok()?;
                        Some((key.to_owned(), nonce, exp))
                    })();
                    match parsed {
                        Some((key, nonce, exp)) if parts.next().is_none() => {
                            if exp.saturating_add(NONCE_SKEW) >= now {
                                entries.insert((key, nonce), exp);
                            } else {
                                dropped = true;
                            }
                        }
                        _ if i + 1 == lines.len() && !complete => dropped = true,
                        _ => {
                            return Err(AgentError::StateIo(
                                "the nonce window file is corrupt".to_owned(),
                            ));
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(AgentError::StateIo(
                    "cannot read the nonce window".to_owned(),
                ));
            }
        }
        let store = Self {
            path,
            entries: Mutex::new(entries),
        };
        if dropped {
            store.rewrite()?;
        }
        Ok(store)
    }

    fn rewrite(&self) -> Result<(), AgentError> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| AgentError::StateIo("nonce lock poisoned".to_owned()))?;
        let mut text = String::new();
        for ((key, nonce), exp) in entries.iter() {
            text.push_str(&format!("{key} {} {exp}\n", hex16(nonce)));
        }
        atomic_write(&self.path, text.as_bytes())
    }

    /// How many nonces are live.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }

    /// Whether the window is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn claim(
        &self,
        key: &KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> Result<bool, StoreError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| StoreError::Backend("nonce lock poisoned".to_owned()))?;
        entries.retain(|_, exp| exp.saturating_add(NONCE_SKEW) >= now);
        let slot = (key.as_str().to_owned(), nonce);
        if entries.contains_key(&slot) {
            return Ok(false);
        }
        if entries.len() >= NONCE_CAP {
            return Err(StoreError::Full);
        }
        let line = format!("{} {} {expires_at}\n", slot.0, hex16(&nonce));
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.path)
            .map_err(|_| StoreError::Unavailable)?;
        file.write_all(line.as_bytes())
            .map_err(|_| StoreError::Unavailable)?;
        file.sync_all().map_err(|_| StoreError::Unavailable)?;
        entries.insert(slot, expires_at);
        Ok(true)
    }
}

impl NonceStore for FileNonceStore {
    fn check_and_insert<'a>(
        &'a self,
        key: &'a KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> NonceFuture<'a, Result<bool, StoreError>> {
        Box::pin(std::future::ready(self.claim(key, nonce, expires_at, now)))
    }
}

impl std::fmt::Debug for FileNonceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileNonceStore")
            .field("live", &self.len())
            .finish()
    }
}

/// The path of the window file inside a state directory.
#[must_use]
pub fn nonce_path(root: &Path) -> PathBuf {
    root.join("nonces.log")
}
