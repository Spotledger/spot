//! The local spool: where a scrubbed crash report waits. The user can list, view, approve and
//! delete what is in it; nothing leaves the machine from here without going through the
//! consent rules.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::crash::CrashReport;
use crate::error::{SupportError, codes};

/// One spooled report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpoolEntry {
    /// The id (16 lower-case hex characters).
    pub id: String,
    /// The size of the file in bytes.
    pub bytes: u64,
    /// Whether the user approved uploading this report.
    pub approved: bool,
}

/// The most reports a spool keeps by default; the oldest go first.
pub const DEFAULT_MAX_REPORTS: usize = 50;
/// The most bytes a spool keeps by default.
pub const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// A folder of spooled reports, bounded in count and in bytes so a crash loop cannot fill the
/// disk: writing past a bound deletes the oldest reports first. Files are private to the
/// user (mode 0600, folder 0700) on Unix.
#[derive(Clone, Debug)]
pub struct Spool {
    dir: PathBuf,
    max_reports: usize,
    max_bytes: u64,
}

fn failed(e: &std::io::Error) -> SupportError {
    SupportError::new(codes::SPOOL_FAILED, e.kind().to_string())
}

impl Spool {
    /// A spool in `dir`; the folder is created on the first write.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_limits(dir, DEFAULT_MAX_REPORTS, DEFAULT_MAX_BYTES)
    }

    /// A spool with its own bounds (at least one report is always kept).
    #[must_use]
    pub fn with_limits(dir: impl Into<PathBuf>, max_reports: usize, max_bytes: u64) -> Self {
        Self {
            dir: dir.into(),
            max_reports: max_reports.max(1),
            max_bytes,
        }
    }

    /// The spool folder.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str, ext: &str) -> Result<PathBuf, SupportError> {
        let ok = id.len() == 16
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !ok {
            return Err(SupportError::new(codes::SPOOL_FAILED, "not a spool id"));
        }
        Ok(self.dir.join(format!("{id}.{ext}")))
    }

    /// Writes an already-scrubbed report and returns its id (a digest of the content).
    ///
    /// # Errors
    /// `report_too_large` or `spool_failed`.
    pub fn write(&self, report: &CrashReport) -> Result<String, SupportError> {
        let bytes = report.to_bytes()?;
        let digest = Sha256::digest(&bytes);
        let id: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        create_private_dir(&self.dir).map_err(|e| failed(&e))?;
        write_private(&self.path(&id, "json")?, &bytes).map_err(|e| failed(&e))?;
        self.enforce_limits(&id)?;
        Ok(id)
    }

    /// Deletes the oldest reports (by file time, then id) until the count and byte bounds
    /// hold, never the one just written.
    fn enforce_limits(&self, keep: &str) -> Result<(), SupportError> {
        let mut entries = Vec::new();
        for entry in self.list()? {
            let modified = fs::metadata(self.path(&entry.id, "json")?)
                .and_then(|m| m.modified())
                .ok();
            entries.push((modified, entry));
        }
        entries.sort_by(|a, b| (a.0, &a.1.id).cmp(&(b.0, &b.1.id)));
        let mut count = entries.len();
        let mut total: u64 = entries.iter().map(|(_, e)| e.bytes).sum();
        for (_, entry) in &entries {
            if count <= self.max_reports && total <= self.max_bytes {
                break;
            }
            if entry.id == keep {
                continue;
            }
            self.delete(&entry.id)?;
            count -= 1;
            total = total.saturating_sub(entry.bytes);
        }
        Ok(())
    }

    /// Every spooled report, sorted by id.
    ///
    /// # Errors
    /// `spool_failed` when the folder cannot be read (a missing folder is an empty spool).
    pub fn list(&self) -> Result<Vec<SpoolEntry>, SupportError> {
        let read = match fs::read_dir(&self.dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(failed(&e)),
        };
        let mut out = Vec::new();
        for item in read {
            let item = item.map_err(|e| failed(&e))?;
            let name = item.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".json")
                && self.path(id, "json").is_ok()
            {
                out.push(SpoolEntry {
                    id: id.to_owned(),
                    bytes: item.metadata().map_err(|e| failed(&e))?.len(),
                    approved: self.path(id, "ok")?.is_file(),
                });
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// The exact text of a spooled report, so the user can see what would be sent.
    ///
    /// # Errors
    /// `spool_failed` when there is no such report.
    pub fn view(&self, id: &str) -> Result<String, SupportError> {
        fs::read_to_string(self.path(id, "json")?).map_err(|e| failed(&e))
    }

    /// Reads a report back as a value.
    ///
    /// # Errors
    /// `spool_failed` when there is no such report or it does not parse.
    pub fn read(&self, id: &str) -> Result<CrashReport, SupportError> {
        serde_json::from_str(&self.view(id)?)
            .map_err(|_| SupportError::new(codes::SPOOL_FAILED, "the report does not parse"))
    }

    /// Marks one report as approved for upload (the answer to an `ask`).
    ///
    /// # Errors
    /// `spool_failed` when there is no such report.
    pub fn approve(&self, id: &str) -> Result<(), SupportError> {
        if !self.path(id, "json")?.is_file() {
            return Err(SupportError::new(codes::SPOOL_FAILED, "no such report"));
        }
        write_private(&self.path(id, "ok")?, b"").map_err(|e| failed(&e))
    }

    /// Deletes one report (and its approval). Deleting a report that is not there is fine.
    ///
    /// # Errors
    /// `spool_failed` when a file cannot be removed.
    pub fn delete(&self, id: &str) -> Result<(), SupportError> {
        for ext in ["json", "ok"] {
            match fs::remove_file(self.path(id, ext)?) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(failed(&e)),
            }
        }
        Ok(())
    }

    /// Deletes every report; returns how many there were.
    ///
    /// # Errors
    /// `spool_failed` when a file cannot be removed.
    pub fn clear(&self) -> Result<usize, SupportError> {
        let entries = self.list()?;
        for entry in &entries {
            self.delete(&entry.id)?;
        }
        Ok(entries.len())
    }
}

/// Creates the folder (and parents), private to the user on Unix.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Writes a file, private to the user on Unix.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}
