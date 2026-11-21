//! The heartbeat outbox: a bounded queue of batches in files in the data directory (WP-CTL-03.T3).
//!
//! It is a file queue on purpose, not a database table: the site's database may be the very thing
//! that is down, and a heartbeat that says so must still be queued. One batch is one file named by
//! a zero-padded sequence number, written to a temporary name and renamed into place, so a crash
//! leaves either the whole batch or nothing. Shipping goes oldest first and a batch is removed only
//! after the control plane accepted it, so a restart neither loses nor repeats a batch.
//!
//! # Bound
//!
//! At most `max_batches` files wait. A full outbox drops the **oldest** batches (the newest
//! observations are the ones that say what the host is now) and keeps a single gap marker: how many
//! batches were dropped and the time span they covered. The marker is local (the wire format has no
//! place for it, by design: the control plane accepts the closed signal set and nothing more); it
//! feeds the readiness detail and the `gap` line of the operator guide.
//!
//! Directory files are only ever named by this module; a foreign file in the directory is ignored.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Batch file extension.
const EXT: &str = "hb";
/// Temporary extension, never read back.
const TMP: &str = "tmp";
/// The gap marker file name.
const GAP_FILE: &str = "gap";

/// What was dropped because the outbox was full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gap {
    /// Batches dropped since the marker began.
    pub batches: u64,
    /// The `observed_at` of the oldest dropped batch (Unix seconds).
    pub from: i64,
    /// The `observed_at` of the newest dropped batch.
    pub until: i64,
}

/// What a push did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pushed {
    /// Old batches dropped to make room.
    pub dropped: u64,
    /// Whether the outbox is now at its bound.
    pub full: bool,
}

struct Inner {
    next: u64,
}

/// The queue.
pub struct FileOutbox {
    dir: PathBuf,
    max_batches: usize,
    inner: Mutex<Inner>,
}

fn io_err(what: &str) -> String {
    format!("the heartbeat outbox {what}")
}

impl FileOutbox {
    /// Opens (creating the directory when needed) the queue in `dir`, holding at most
    /// `max_batches` batches.
    ///
    /// # Errors
    /// A fixed sentence when the directory cannot be created or read.
    pub fn open(dir: &Path, max_batches: usize) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|_| io_err("directory cannot be created"))?;
        let outbox = Self {
            dir: dir.to_path_buf(),
            max_batches: max_batches.max(1),
            inner: Mutex::new(Inner { next: 0 }),
        };
        let highest = outbox.sequences()?.last().copied();
        outbox.lock()?.next = highest.map_or(0, |n| n.saturating_add(1));
        // A torn write from a crash never became a batch: remove it.
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.path().extension().and_then(|e| e.to_str()) == Some(TMP) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(outbox)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, String> {
        self.inner.lock().map_err(|_| io_err("is unavailable"))
    }

    fn path_of(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq:020}.{EXT}"))
    }

    fn sequences(&self) -> Result<Vec<u64>, String> {
        let mut found = Vec::new();
        for entry in fs::read_dir(&self.dir)
            .map_err(|_| io_err("cannot be read"))?
            .flatten()
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(EXT) {
                continue;
            }
            if let Some(n) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
            {
                found.push(n);
            }
        }
        found.sort_unstable();
        Ok(found)
    }

    /// Batches waiting.
    ///
    /// # Errors
    /// A fixed sentence.
    pub fn len(&self) -> Result<usize, String> {
        Ok(self.sequences()?.len())
    }

    /// Whether nothing waits.
    ///
    /// # Errors
    /// A fixed sentence.
    pub fn is_empty(&self) -> Result<bool, String> {
        Ok(self.len()? == 0)
    }

    /// Queues one batch body observed at `at` (Unix seconds), dropping the oldest batches when
    /// the bound is reached.
    ///
    /// # Errors
    /// A fixed sentence when the batch could not be written; nothing is queued then.
    pub fn push(&self, body: &[u8], at: i64) -> Result<Pushed, String> {
        let mut inner = self.lock()?;
        let seq = inner.next;
        let tmp = self.dir.join(format!("{seq:020}.{TMP}"));
        let mut file = fs::File::create(&tmp).map_err(|_| io_err("cannot be written"))?;
        // The body is a heartbeat: counters and integers only, but keep it private to the owner.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }
        file.write_all(body)
            .and_then(|()| file.sync_all())
            .map_err(|_| {
                let _ = fs::remove_file(&tmp);
                io_err("cannot be written")
            })?;
        drop(file);
        fs::rename(&tmp, self.path_of(seq)).map_err(|_| {
            let _ = fs::remove_file(&tmp);
            io_err("cannot be written")
        })?;
        inner.next = seq.saturating_add(1);
        drop(inner);

        let mut dropped = 0_u64;
        let mut seqs = self.sequences()?;
        while seqs.len() > self.max_batches {
            let oldest = seqs.remove(0);
            let when = self.observed_at(oldest).unwrap_or(at);
            if fs::remove_file(self.path_of(oldest)).is_ok() {
                dropped = dropped.saturating_add(1);
                self.extend_gap(when);
            } else {
                break;
            }
        }
        Ok(Pushed {
            dropped,
            full: seqs.len() >= self.max_batches,
        })
    }

    /// The oldest waiting batch: its sequence number and body.
    ///
    /// # Errors
    /// A fixed sentence.
    pub fn oldest(&self) -> Result<Option<(u64, Vec<u8>)>, String> {
        let Some(seq) = self.sequences()?.first().copied() else {
            return Ok(None);
        };
        let body = fs::read(self.path_of(seq)).map_err(|_| io_err("cannot be read"))?;
        Ok(Some((seq, body)))
    }

    /// Removes a batch (accepted or refused for good).
    ///
    /// # Errors
    /// A fixed sentence.
    pub fn remove(&self, seq: u64) -> Result<(), String> {
        match fs::remove_file(self.path_of(seq)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(io_err("cannot be updated")),
        }
    }

    /// The gap marker, when batches were dropped.
    #[must_use]
    pub fn gap(&self) -> Option<Gap> {
        let text = fs::read_to_string(self.dir.join(GAP_FILE)).ok()?;
        let mut parts = text.split_whitespace().map(str::parse::<i64>);
        let batches = parts.next()?.ok()?;
        let from = parts.next()?.ok()?;
        let until = parts.next()?.ok()?;
        Some(Gap {
            batches: u64::try_from(batches).ok()?,
            from,
            until,
        })
    }

    /// Clears the marker once the gap has been reported shipped past (the next accepted batch).
    pub fn clear_gap(&self) {
        let _ = fs::remove_file(self.dir.join(GAP_FILE));
    }

    fn extend_gap(&self, when: i64) {
        let gap = self.gap().unwrap_or(Gap {
            batches: 0,
            from: when,
            until: when,
        });
        let next = Gap {
            batches: gap.batches.saturating_add(1),
            from: gap.from.min(when),
            until: gap.until.max(when),
        };
        let text = format!("{} {} {}", next.batches, next.from, next.until);
        let tmp = self.dir.join(format!("{GAP_FILE}.{TMP}"));
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(&tmp, self.dir.join(GAP_FILE));
        }
    }

    /// The `observed_at` of the first observation in batch `seq`, read without keeping the body.
    fn observed_at(&self, seq: u64) -> Option<i64> {
        let body = fs::read(self.path_of(seq)).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&body).ok()?;
        value
            .get("observations")?
            .as_array()?
            .first()?
            .get("observed_at")?
            .as_i64()
    }
}
