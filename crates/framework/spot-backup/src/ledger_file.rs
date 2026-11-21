//! A durable [`BackupLedger`]: one `ledger.toml` in a directory (WP-PRD-04.T6).
//!
//! Retention treats the ledger as the only source of truth for "verified", so it must
//! survive the process and must never silently become empty:
//!
//! * every change rewrites the whole file: serialise to `ledger.toml.tmp-<random>` in
//!   the same directory (created with `create_new`, mode 0600 on Unix), `sync_all`,
//!   `rename` over `ledger.toml`, then `sync_all` the directory on Unix. A crash leaves
//!   the old file or the new file, never a torn one;
//! * a leftover `*.tmp-*` file from a crash is ignored on load and removed on the next
//!   write;
//! * a missing file is an empty ledger, but an unreadable, unparsable, duplicated or
//!   unknown-format file is an **error** ([`BackupError::LedgerCorrupt`]): treating a
//!   corrupt ledger as empty would let retention delete the only verified backup;
//! * a write failure is returned by [`FileLedger::try_upsert`] / [`FileLedger::try_remove`];
//!   the infallible trait methods record it for [`FileLedger::health`] instead.
//!
//! [`FileLedger::lock`] takes an exclusive `ledger.lock` file so two runs cannot share
//! a ledger. A lock left by a crash is the caller's concern: the error names the owning
//! process id when it can be read.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use rand::TryRngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{BackupError, Result};
use crate::model::BackupRecord;
use crate::runner::BackupLedger;

/// The ledger's file name inside its directory.
pub const LEDGER_FILE: &str = "ledger.toml";
/// The lock file's name inside the ledger directory.
pub const LOCK_FILE: &str = "ledger.lock";
/// The `format` value this build reads and writes.
pub const LEDGER_FORMAT: u32 = 1;

const TEMP_PREFIX: &str = "ledger.toml.tmp-";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerDoc {
    format: u32,
    #[serde(default, rename = "record")]
    records: Vec<RecordRow>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordRow {
    id: String,
    created_at: String,
    verified: bool,
    sequence: u64,
    open_restore: bool,
}

impl RecordRow {
    fn from_record(record: &BackupRecord) -> Result<RecordRow> {
        let created_at = record
            .created_at
            .to_offset(time::UtcOffset::UTC)
            .format(&Rfc3339)
            .map_err(|error| BackupError::LedgerWriteFailed {
                path: LEDGER_FILE.to_owned(),
                reason: format!("a record's time cannot be rendered: {error}"),
            })?;
        Ok(RecordRow {
            id: record.id.clone(),
            created_at,
            verified: record.verified,
            sequence: record.sequence,
            open_restore: record.open_restore,
        })
    }

    fn into_record(self, path: &Path) -> Result<BackupRecord> {
        let created_at = OffsetDateTime::parse(&self.created_at, &Rfc3339)
            .map_err(|_| corrupt(path, format!("record {:?} has an invalid time", self.id)))?;
        Ok(BackupRecord {
            id: self.id,
            created_at,
            verified: self.verified,
            sequence: self.sequence,
            open_restore: self.open_restore,
        })
    }
}

fn corrupt(path: &Path, reason: impl Into<String>) -> BackupError {
    BackupError::LedgerCorrupt {
        path: path.display().to_string(),
        reason: reason.into(),
    }
}

fn write_failed(path: &Path, reason: impl Into<String>) -> BackupError {
    BackupError::LedgerWriteFailed {
        path: path.display().to_string(),
        reason: reason.into(),
    }
}

/// What a ledger knows about its own write failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerHealth {
    /// False once any write failed, until [`FileLedger::clear_health`].
    pub healthy: bool,
    /// The code of the last write failure (`backup.ledger_write_failed`), if any.
    pub last_error: Option<&'static str>,
}

/// A ledger persisted to `ledger.toml` in one directory.
#[derive(Debug)]
pub struct FileLedger {
    dir: PathBuf,
    records: Mutex<Vec<BackupRecord>>,
    failed: AtomicBool,
    last_error: Mutex<Option<&'static str>>,
}

impl FileLedger {
    /// Opens the ledger in `dir`. A missing file is an empty ledger.
    ///
    /// # Errors
    /// [`BackupError::LedgerCorrupt`] when the file cannot be read, does not parse,
    /// names an unknown format, repeats an id or holds a bad record. The file is never
    /// modified or truncated by a failed open.
    pub fn open(dir: impl Into<PathBuf>) -> Result<FileLedger> {
        let dir = dir.into();
        let path = dir.join(LEDGER_FILE);
        let records = match std::fs::read_to_string(&path) {
            Ok(text) => parse(&path, &text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(corrupt(&path, format!("the file cannot be read: {error}")));
            }
        };
        Ok(FileLedger {
            dir,
            records: Mutex::new(records),
            failed: AtomicBool::new(false),
            last_error: Mutex::new(None),
        })
    }

    /// Takes the exclusive `ledger.lock` in `dir` (created when missing). The lock is
    /// released when the returned guard is dropped.
    ///
    /// # Errors
    /// [`BackupError::LedgerLocked`] when the lock file exists (naming its owner's
    /// process id when readable), [`BackupError::Io`] on any other failure.
    pub fn lock(dir: impl AsRef<Path>) -> Result<LedgerLock> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let path = dir.join(LOCK_FILE);
        match private_create(&path) {
            Ok(mut file) => {
                let written =
                    writeln!(file, "{}", std::process::id()).and_then(|()| file.sync_all());
                if let Err(error) = written {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(error.into());
                }
                Ok(LedgerLock { path })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let owner = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok());
                Err(BackupError::LedgerLocked {
                    path: path.display().to_string(),
                    owner,
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The directory holding `ledger.toml`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Inserts or replaces the record for its id and persists the ledger.
    ///
    /// # Errors
    /// [`BackupError::LedgerWriteFailed`]; the in-memory ledger is left as it was.
    pub fn try_upsert(&self, record: BackupRecord) -> Result<()> {
        self.mutate(|records| {
            records.retain(|existing| existing.id != record.id);
            records.push(record);
        })
    }

    /// Removes a record and persists the ledger. Removing an unknown id is `Ok`.
    ///
    /// # Errors
    /// [`BackupError::LedgerWriteFailed`]; the in-memory ledger is left as it was.
    pub fn try_remove(&self, id: &str) -> Result<()> {
        self.mutate(|records| records.retain(|existing| existing.id != id))
    }

    /// Whether any write failed since the ledger was opened or the health was cleared.
    pub fn health(&self) -> LedgerHealth {
        let last_error = *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        LedgerHealth {
            healthy: !self.failed.load(Ordering::SeqCst),
            last_error,
        }
    }

    /// Clears a recorded write failure (the host calls it once it has acted on it).
    pub fn clear_health(&self) {
        self.failed.store(false, Ordering::SeqCst);
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, Vec<BackupRecord>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Applies `change` to a copy, persists the copy, and only then adopts it, so the
    /// in-memory ledger never gets ahead of the file. The lock is held across the
    /// write, so concurrent changes in one process are serialised.
    fn mutate(&self, change: impl FnOnce(&mut Vec<BackupRecord>)) -> Result<()> {
        let mut guard = self.guard();
        let mut next = guard.clone();
        change(&mut next);
        match self.persist(&next) {
            Ok(()) => {
                *guard = next;
                Ok(())
            }
            Err(error) => {
                self.failed.store(true, Ordering::SeqCst);
                *self
                    .last_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.code());
                Err(error)
            }
        }
    }

    fn persist(&self, records: &[BackupRecord]) -> Result<()> {
        let final_path = self.dir.join(LEDGER_FILE);
        std::fs::create_dir_all(&self.dir).map_err(|e| write_failed(&final_path, e.to_string()))?;
        remove_stale_temps(&self.dir);

        let mut rows = records
            .iter()
            .map(RecordRow::from_record)
            .collect::<Result<Vec<_>>>()?;
        rows.sort_by(|left, right| left.id.cmp(&right.id));
        let text = toml::to_string_pretty(&LedgerDoc {
            format: LEDGER_FORMAT,
            records: rows,
        })
        .map_err(|error| {
            write_failed(
                &final_path,
                format!("the ledger cannot be rendered: {error}"),
            )
        })?;

        let tmp_path = self.dir.join(format!("{TEMP_PREFIX}{}", random_suffix()?));
        let mut file = private_create(&tmp_path)
            .map_err(|error| write_failed(&tmp_path, error.to_string()))?;
        let written = file
            .write_all(text.as_bytes())
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = written {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(write_failed(&tmp_path, error.to_string()));
        }
        if let Err(error) = std::fs::rename(&tmp_path, &final_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(write_failed(&final_path, error.to_string()));
        }
        sync_dir(&self.dir).map_err(|error| write_failed(&self.dir, error.to_string()))
    }
}

impl BackupLedger for FileLedger {
    fn records(&self) -> Vec<BackupRecord> {
        let mut records = self.guard().clone();
        records.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.sequence.cmp(&left.sequence))
        });
        records
    }

    fn last_verified(&self) -> Option<BackupRecord> {
        self.records().into_iter().find(|record| record.verified)
    }

    fn upsert(&self, record: BackupRecord) {
        // The failure is recorded for `health()` inside `mutate`.
        let _ = self.try_upsert(record);
    }

    fn remove(&self, id: &str) {
        let _ = self.try_remove(id);
    }

    fn try_upsert(&self, record: BackupRecord) -> Result<()> {
        self.mutate(|records| {
            records.retain(|existing| existing.id != record.id);
            records.push(record);
        })
    }

    fn try_remove(&self, id: &str) -> Result<()> {
        self.mutate(|records| records.retain(|existing| existing.id != id))
    }
}

/// An exclusive lock on a ledger directory; the lock file is removed on drop.
#[derive(Debug)]
pub struct LedgerLock {
    path: PathBuf,
}

impl LedgerLock {
    /// The lock file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn parse(path: &Path, text: &str) -> Result<Vec<BackupRecord>> {
    let doc: LedgerDoc = toml::from_str(text)
        .map_err(|error| corrupt(path, format!("the file does not parse: {error}")))?;
    if doc.format != LEDGER_FORMAT {
        return Err(corrupt(
            path,
            format!(
                "the ledger format {} is not {LEDGER_FORMAT}; this build cannot read it",
                doc.format
            ),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut records = Vec::with_capacity(doc.records.len());
    for row in doc.records {
        if row.id.trim().is_empty() {
            return Err(corrupt(path, "a record has an empty id"));
        }
        if !seen.insert(row.id.clone()) {
            return Err(corrupt(path, format!("record {:?} appears twice", row.id)));
        }
        records.push(row.into_record(path)?);
    }
    Ok(records)
}

/// Creates a new file for writing, mode 0600 on Unix, failing if it exists.
fn private_create(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn random_suffix() -> Result<String> {
    let mut bytes = [0u8; 8];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| BackupError::io("the operating system RNG is unavailable"))?;
    let mut out = String::with_capacity(16);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// Removes `ledger.toml.tmp-*` files a crashed write left behind (best effort).
fn remove_stale_temps(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(TEMP_PREFIX) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    // `std` has no portable directory handle; the file was synced before the rename.
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::retention::{RetentionPolicy, plan};
    use time::Duration;
    use time::macros::datetime;

    fn record(id: &str, at: OffsetDateTime, verified: bool, sequence: u64) -> BackupRecord {
        BackupRecord {
            id: id.to_owned(),
            created_at: at,
            verified,
            sequence,
            open_restore: false,
        }
    }

    #[test]
    fn upsert_and_reload_gives_the_same_records() {
        let dir = tempfile::tempdir().expect("dir");
        let now = datetime!(2026-10-19 12:00:00.123456789 UTC);
        let ledger = FileLedger::open(dir.path()).expect("open empty");
        assert!(ledger.records().is_empty());
        ledger.try_upsert(record("b1", now, true, 1)).expect("b1");
        ledger
            .try_upsert(record("b2", now + Duration::days(1), false, 2))
            .expect("b2");
        ledger
            .try_upsert(record("b1", now, false, 1))
            .expect("replace b1");
        ledger.try_remove("unknown").expect("unknown id is fine");

        let reloaded = FileLedger::open(dir.path()).expect("reopen");
        assert_eq!(reloaded.records(), ledger.records());
        assert_eq!(reloaded.records().len(), 2);
        assert!(reloaded.last_verified().is_none());
        assert!(ledger.health().healthy);

        ledger.try_remove("b2").expect("remove");
        let reloaded = FileLedger::open(dir.path()).expect("reopen again");
        assert_eq!(reloaded.records().len(), 1);
    }

    #[test]
    fn a_torn_temp_file_is_ignored_then_removed_on_the_next_write() {
        let dir = tempfile::tempdir().expect("dir");
        let torn = dir.path().join(format!("{TEMP_PREFIX}deadbeef"));
        std::fs::write(&torn, "format = 1\n[[rec").expect("torn file");
        let ledger = FileLedger::open(dir.path()).expect("a torn temp does not matter");
        assert!(ledger.records().is_empty());
        assert!(torn.exists(), "open must not touch it");
        ledger
            .try_upsert(record("b1", datetime!(2026-10-19 12:00:00 UTC), true, 1))
            .expect("write");
        assert!(!torn.exists(), "the next write removes it");
        assert!(dir.path().join(LEDGER_FILE).exists());
    }

    #[test]
    fn a_corrupt_ledger_is_an_error_and_is_never_truncated() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(LEDGER_FILE);
        for bad in [
            "format = 1\n[[record]]\nid = \"x\"",
            "not toml at all {{{",
            "format = 2\n",
            "format = 1\n[[record]]\nid = \"a\"\ncreated_at = \"2026-10-19T12:00:00Z\"\nverified = true\nsequence = 1\nopen_restore = false\n[[record]]\nid = \"a\"\ncreated_at = \"2026-10-19T12:00:00Z\"\nverified = true\nsequence = 2\nopen_restore = false\n",
            "format = 1\n[[record]]\nid = \"a\"\ncreated_at = \"yesterday\"\nverified = true\nsequence = 1\nopen_restore = false\n",
            "",
        ] {
            std::fs::write(&path, bad).expect("write bad ledger");
            let error = FileLedger::open(dir.path()).expect_err("must fail closed");
            assert_eq!(error.code(), "backup.ledger_corrupt", "input: {bad:?}");
            assert_eq!(std::fs::read_to_string(&path).expect("read"), bad);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_directory_fails_the_write_and_reports_health() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("dir");
        let ledger = FileLedger::open(dir.path()).expect("open");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555))
            .expect("chmod");
        // A superuser ignores the mode; the test only means something for a normal user.
        let probe = dir.path().join("probe");
        if std::fs::write(&probe, b"x").is_ok() {
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
                .expect("restore mode");
            return;
        }
        let error = ledger
            .try_upsert(record("b1", datetime!(2026-10-19 12:00:00 UTC), true, 1))
            .expect_err("a read-only directory");
        assert_eq!(error.code(), "backup.ledger_write_failed");
        // The infallible trait method records the failure instead of returning it.
        let trait_ledger: &dyn BackupLedger = &ledger;
        trait_ledger.upsert(record("b2", datetime!(2026-10-19 12:00:00 UTC), true, 2));
        assert!(
            trait_ledger
                .try_upsert(record("b3", datetime!(2026-10-19 12:00:00 UTC), true, 3))
                .is_err()
        );
        let health = ledger.health();
        assert!(!health.healthy);
        assert_eq!(health.last_error, Some("backup.ledger_write_failed"));
        assert!(
            ledger.records().is_empty(),
            "memory never gets ahead of the file"
        );
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("restore mode");
        ledger.clear_health();
        assert!(ledger.health().healthy);
    }

    #[test]
    fn a_second_lock_is_refused_and_the_first_releases_on_drop() {
        let dir = tempfile::tempdir().expect("dir");
        let first = FileLedger::lock(dir.path()).expect("first lock");
        let error = FileLedger::lock(dir.path()).expect_err("second lock");
        assert_eq!(error.code(), "backup.ledger_locked");
        assert!(matches!(
            error,
            BackupError::LedgerLocked { owner: Some(pid), .. } if pid == std::process::id()
        ));
        drop(first);
        let again = FileLedger::lock(dir.path()).expect("lock after release");
        assert!(again.path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_ledger_file_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("dir");
        let ledger = FileLedger::open(dir.path()).expect("open");
        ledger
            .try_upsert(record("b1", datetime!(2026-10-19 12:00:00 UTC), true, 1))
            .expect("write");
        let mode = std::fs::metadata(dir.path().join(LEDGER_FILE))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn retention_planned_from_a_reloaded_ledger_keeps_the_last_verified_backup() {
        let dir = tempfile::tempdir().expect("dir");
        let now = datetime!(2026-10-19 12:00:00 UTC);
        let ledger = FileLedger::open(dir.path()).expect("open");
        ledger
            .try_upsert(record("old-good", now - Duration::days(500), true, 1))
            .expect("old-good");
        ledger
            .try_upsert(record("newer-failed", now - Duration::days(400), false, 2))
            .expect("newer-failed");
        drop(ledger);

        let reloaded = FileLedger::open(dir.path()).expect("reopen");
        let records = reloaded.records();
        let policy = RetentionPolicy {
            keep_daily: 1,
            keep_weekly: 1,
            keep_monthly: 1,
            protect_newer_than_days: 1,
            keep_last: 0,
        };
        let planned = plan(&policy, &records, now).expect("plan");
        assert!(planned.keeps("old-good"));
        assert!(planned.delete.contains(&"newer-failed".to_owned()));
    }
}
