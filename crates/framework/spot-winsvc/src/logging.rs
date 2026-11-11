//! File logging with rotation, redacted on the way out (brief "Directories": "logs
//! there with rotation"; "Security notes": "logs redact (spot-redact)").
//!
//! This module ships the two portable pieces the brief's logging deliverable needs:
//! [`RotatingFileWriter`] (size-based rotation, a plain [`std::io::Write`], fully
//! Linux-tested) and [`RedactingFileLogger`], which runs every write through
//! [`spot_redact::Redactor`] before it reaches the file. What it does **not** attempt is
//! rewiring every `tracing::info!`/`error!` call site across the workspace through a
//! structured subscriber: that needs `tracing-subscriber` (and, for its own file
//! rotation, typically `tracing-appender`) as new dependencies, which this brief does not
//! name and the project coding rules forbid adding without the lead's say — see the task's
//! `## Open questions`. [`crate::run::run_foreground`]'s own diagnostics (today:
//! `eprintln!`/the `on_ready` callback) can write through a [`RedactingFileLogger`]
//! directly without that dependency; a full `tracing` subscriber migration is a
//! follow-up this task flags rather than silently half-implements.

use crate::error::WinsvcError;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A [`std::io::Write`] over one log file that rotates to `<name>.1`, `<name>.2`, ... once
/// the current file would exceed `max_bytes`, keeping at most `max_backups` old files
/// (the oldest is deleted). Every method is synchronized by an internal [`Mutex`], so one
/// instance can be shared (e.g. through an `Arc`) across the service's few log call
/// sites without each needing its own lock.
pub struct RotatingFileWriter {
    inner: Mutex<RotatingState>,
}

struct RotatingState {
    path: PathBuf,
    max_bytes: u64,
    max_backups: u32,
    file: File,
    written: u64,
}

impl RotatingFileWriter {
    /// Opens (creating if needed) the log file at `path` for appending, rotating
    /// immediately if it is already at or over `max_bytes`.
    ///
    /// # Errors
    /// [`WinsvcError::Io`] on any filesystem failure (creating the parent directory,
    /// opening the file, or an initial rotation).
    pub fn open(path: &Path, max_bytes: u64, max_backups: u32) -> Result<Self, WinsvcError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| WinsvcError::io("creating log directory", &e))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| WinsvcError::io("opening log file", &e))?;
        let written = file
            .metadata()
            .map_err(|e| WinsvcError::io("reading log file metadata", &e))?
            .len();
        let mut state = RotatingState {
            path: path.to_path_buf(),
            max_bytes,
            max_backups,
            file,
            written,
        };
        if state.written >= state.max_bytes {
            state.rotate()?;
        }
        Ok(RotatingFileWriter {
            inner: Mutex::new(state),
        })
    }
}

impl RotatingState {
    /// `<name>.N` for backup `n` (1-based; `.1` is the newest backup).
    fn backup_path(&self, n: u32) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(format!(".{n}"));
        self.path.with_file_name(name)
    }

    /// Shifts every existing backup up by one slot (deleting whatever would spill past
    /// `max_backups`), moves the current file to `.1`, then opens a fresh current file.
    fn rotate(&mut self) -> Result<(), WinsvcError> {
        if self.max_backups == 0 {
            // No backups kept: truncate the current file in place instead of renaming it
            // away, so there is always exactly one log file.
            self.file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)
                .map_err(|e| WinsvcError::io("truncating log file", &e))?;
            self.written = 0;
            return Ok(());
        }
        let oldest = self.backup_path(self.max_backups);
        if oldest.exists() {
            std::fs::remove_file(&oldest)
                .map_err(|e| WinsvcError::io("removing the oldest log backup", &e))?;
        }
        let mut n = self.max_backups;
        while n > 1 {
            let from = self.backup_path(n - 1);
            if from.exists() {
                std::fs::rename(&from, self.backup_path(n))
                    .map_err(|e| WinsvcError::io("rotating a log backup", &e))?;
            }
            n -= 1;
        }
        std::fs::rename(&self.path, self.backup_path(1))
            .map_err(|e| WinsvcError::io("rotating the current log file", &e))?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| WinsvcError::io("reopening log file after rotation", &e))?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.written + buf.len() as u64 > state.max_bytes {
            state
                .rotate()
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        state.file.write_all(buf)?;
        state.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .file
            .flush()
    }
}

/// Wraps a [`RotatingFileWriter`] (or any other [`Write`]) with [`spot_redact::Redactor`]:
/// every write is redacted first, line-oriented sinks never see a secret even if a log
/// call site accidentally formats one in (the brief's "logs redact (spot-redact)").
pub struct RedactingFileLogger<W> {
    redactor: spot_redact::Redactor,
    inner: W,
}

impl<W: Write> RedactingFileLogger<W> {
    /// Wraps `inner` with the default [`spot_redact::RedactionPolicy`].
    ///
    /// # Errors
    /// Whatever [`spot_redact::Redactor::from_policy`] returns (a malformed built-in
    /// pattern, which never happens in practice).
    pub fn new(inner: W) -> Result<Self, WinsvcError> {
        let redactor = spot_redact::Redactor::from_policy(&spot_redact::RedactionPolicy::default())
            .map_err(|e| WinsvcError::failed(format!("building the log redactor: {e}")))?;
        Ok(RedactingFileLogger { redactor, inner })
    }

    /// Redacts and writes one already-formatted line, appending `\n` (the common case:
    /// one call per log event, rather than going through [`std::io::Write::write_all`]
    /// with a pre-built buffer).
    ///
    /// # Errors
    /// [`WinsvcError::Io`] on the underlying write.
    pub fn write_line(&mut self, line: &str) -> Result<(), WinsvcError> {
        let redacted = self.redactor.redact_str(line);
        self.inner
            .write_all(redacted.as_bytes())
            .and_then(|()| self.inner.write_all(b"\n"))
            .map_err(|e| WinsvcError::io("writing log line", &e))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn writes_accumulate_in_the_current_file_until_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spot-winsvc.log");
        let mut writer = RotatingFileWriter::open(&path, 1024, 2).expect("open");
        writer.write_all(b"hello\n").expect("write");
        writer.flush().expect("flush");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "hello\n");
        assert!(!path.with_file_name("spot-winsvc.log.1").exists());
    }

    #[test]
    fn a_write_that_would_exceed_max_bytes_rotates_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spot-winsvc.log");
        let mut writer = RotatingFileWriter::open(&path, 10, 2).expect("open");
        writer
            .write_all(b"0123456789")
            .expect("fills exactly to the limit");
        writer
            .write_all(b"more")
            .expect("over the limit, must rotate first");

        assert_eq!(
            std::fs::read_to_string(&path).expect("current file"),
            "more"
        );
        assert_eq!(
            std::fs::read_to_string(path.with_file_name("spot-winsvc.log.1")).expect("backup"),
            "0123456789"
        );
    }

    #[test]
    fn old_backups_beyond_max_backups_are_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spot-winsvc.log");
        let mut writer = RotatingFileWriter::open(&path, 4, 2).expect("open");
        for chunk in [b"aaaa", b"bbbb", b"cccc", b"dddd"] {
            writer.write_all(chunk).expect("write");
        }

        assert_eq!(
            std::fs::read_to_string(&path).expect("current"),
            "dddd",
            "the newest write is always the current file"
        );
        assert_eq!(
            std::fs::read_to_string(path.with_file_name("spot-winsvc.log.1")).expect(".1"),
            "cccc"
        );
        assert_eq!(
            std::fs::read_to_string(path.with_file_name("spot-winsvc.log.2")).expect(".2"),
            "bbbb"
        );
        assert!(
            !path.with_file_name("spot-winsvc.log.3").exists(),
            "max_backups=2 must never keep a third backup"
        );
    }

    #[test]
    fn zero_backups_truncates_in_place_instead_of_renaming() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spot-winsvc.log");
        let mut writer = RotatingFileWriter::open(&path, 4, 0).expect("open");
        writer.write_all(b"aaaa").expect("fills the file");
        writer.write_all(b"bbbb").expect("over the limit");

        assert_eq!(std::fs::read_to_string(&path).expect("current"), "bbbb");
        assert!(!path.with_file_name("spot-winsvc.log.1").exists());
    }

    #[test]
    fn redacting_file_logger_removes_secrets_from_every_written_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spot-winsvc.log");
        let writer = RotatingFileWriter::open(&path, 1024 * 1024, 2).expect("open");
        let mut logger = RedactingFileLogger::new(writer).expect("logger");

        logger
            .write_line("starting up with password=super-secret-value-123")
            .expect("write");

        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(!contents.contains("super-secret-value-123"));
        assert!(contents.contains("password=[redacted:"));
    }
}
