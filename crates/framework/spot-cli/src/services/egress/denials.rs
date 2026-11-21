//! The denial record: one JSON line per refused outbound request, an append-only file, a counter
//! per purpose, and the bounded reader `spot egress denials` uses.
//!
//! What a line holds: when (unix seconds, from the site clock), tenant, purpose, actor, scheme,
//! host, port, the address class and the rule that refused it. What it never holds: a query
//! string, userinfo, a fragment, a header, a body, or the addresses the host resolved to (an
//! internal address is exactly what a tenant must not learn, and the file may be shared with
//! support). The record the guard hands over carries none of the first four; the last is dropped
//! here.
//!
//! The file sink is modelled on `audit::FileAudit`: mode 0600, never a symlink, size-capped with
//! three rotated files kept. Unlike the identity audit it does not `fsync` each line (a denial is
//! a diagnostic, and the sink runs on the request path); a failed write is counted and logged
//! once a minute by kind, and never changes the answer the guard already gave.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use spot_core::Clock;
use spot_egress::{AddrClass, DenialRecord, DenialSink};

use super::METRIC_DENIED;
use super::policy::PURPOSES;

/// The rotated files kept next to the live one.
pub const FILE_KEEP: usize = 3;
/// The most bytes `spot egress denials` reads from one file (its tail).
pub const READ_TAIL_BYTES: u64 = 4 * 1024 * 1024;
/// The most denials one `spot egress denials` call returns.
pub const LIMIT_MAX: usize = 1000;
/// The longest tenant, actor or host kept in a line, in characters.
const FIELD_MAX: usize = 253;
/// The longest line written; a longer one is replaced by a stub.
const LINE_MAX: usize = 2048;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// One line of the denial file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenialLine {
    /// When, in unix seconds.
    pub at: i64,
    /// The tenant (the site name) whose request was refused.
    pub tenant: String,
    /// The purpose: one of the closed set in [`PURPOSES`].
    pub purpose: String,
    /// The actor that made the request, when the caller named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// `http`, `https` or `generic` (an address-based caller such as SMTP).
    pub scheme: String,
    /// The host as the caller wrote it, never a resolved address.
    pub host: String,
    /// The port.
    pub port: u16,
    /// The class of the refused address (`loopback`, `private`, ...); `public` for a refusal
    /// that is not about an address.
    pub class: String,
    /// The rule that refused: `policy`, `address`, `port` or `resolution`.
    pub rule: String,
}

fn clip(text: &str) -> String {
    text.chars().take(FIELD_MAX).collect()
}

/// `CarrierGradeNat` as `carrier_grade_nat`.
pub fn class_name(class: AddrClass) -> String {
    let debug = format!("{class:?}");
    let mut out = String::with_capacity(debug.len() + 4);
    for (i, c) in debug.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

impl DenialLine {
    /// The line for `record`, bounded; the resolved addresses are dropped on purpose.
    pub fn from_record(record: &DenialRecord) -> Self {
        Self {
            at: record.at.unix_timestamp(),
            tenant: clip(&record.tenant),
            purpose: record.purpose.as_str().to_owned(),
            actor: record.actor.as_deref().map(clip),
            scheme: clip(&record.scheme),
            host: clip(&record.host),
            port: record.port,
            class: class_name(record.class),
            rule: clip(&record.rule),
        }
    }
}

// ---- the file sink ---------------------------------------------------------------------------

struct FileState {
    file: Option<std::fs::File>,
    size: u64,
}

fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    // The file is never a link: a planted symlink would redirect the trail elsewhere.
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("the denial log path is a symlink"));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Appends one JSON line per denial to a size-capped file. See the module documentation.
pub struct FileDenialSink {
    path: PathBuf,
    max_bytes: u64,
    clock: Arc<dyn Clock>,
    state: Mutex<FileState>,
    written: AtomicU64,
    failed: AtomicU64,
    healthy: AtomicBool,
    last_logged: Mutex<Option<Duration>>,
}

impl FileDenialSink {
    /// A sink on `path`. A file that cannot be opened now is retried on every denial: the sink
    /// starts either way, reports itself unhealthy and counts the failures.
    pub fn new(path: &Path, max_bytes: u64, clock: Arc<dyn Clock>) -> Self {
        let opened = open_append(path);
        let healthy = opened.is_ok();
        let (file, size) = match opened {
            Ok(file) => {
                let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                (Some(file), size)
            }
            Err(_) => (None, 0),
        };
        Self {
            path: path.to_path_buf(),
            max_bytes: max_bytes.max(1024),
            clock,
            state: Mutex::new(FileState { file, size }),
            written: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            healthy: AtomicBool::new(healthy),
            last_logged: Mutex::new(None),
        }
    }

    /// The file this sink appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lines written since start.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Lines that could not be written since start.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Whether the last open or write worked.
    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    fn rotated(&self, n: usize) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    fn rotate(&self, state: &mut FileState) -> std::io::Result<()> {
        state.file = None;
        let _ = std::fs::remove_file(self.rotated(FILE_KEEP));
        for n in (1..FILE_KEEP).rev() {
            let from = self.rotated(n);
            if from.exists() {
                std::fs::rename(&from, self.rotated(n + 1))?;
            }
        }
        std::fs::rename(&self.path, self.rotated(1))?;
        state.file = Some(open_append(&self.path)?);
        state.size = 0;
        Ok(())
    }

    fn append(&self, line: &[u8]) -> std::io::Result<()> {
        let mut state = lock(&self.state);
        if state.file.is_none() {
            let file = open_append(&self.path)?;
            state.size = file.metadata().map(|m| m.len()).unwrap_or(0);
            state.file = Some(file);
        }
        if state.size.saturating_add(line.len() as u64) > self.max_bytes {
            self.rotate(&mut state)?;
        }
        let Some(file) = state.file.as_mut() else {
            return Err(std::io::Error::other("the denial log is not open"));
        };
        if let Err(e) = file.write_all(line) {
            // Reopen next time: the file may have been moved or the disk may have recovered.
            state.file = None;
            return Err(e);
        }
        state.size += line.len() as u64;
        Ok(())
    }

    fn note_failure(&self, kind: std::io::ErrorKind) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        self.healthy.store(false, Ordering::Relaxed);
        let now = self.clock.monotonic();
        let mut last = lock(&self.last_logged);
        if last.is_none_or(|t| now.saturating_sub(t) >= Duration::from_secs(60)) {
            *last = Some(now);
            tracing::error!(?kind, "the egress denial log could not be written");
        }
    }
}

impl DenialSink for FileDenialSink {
    fn denied(&self, record: DenialRecord) {
        let line = DenialLine::from_record(&record);
        let mut text = match serde_json::to_string(&line) {
            Ok(text) => text,
            Err(_) => {
                self.note_failure(std::io::ErrorKind::InvalidData);
                return;
            }
        };
        if text.len() > LINE_MAX {
            text = format!("{{\"at\":{},\"rule\":\"record_too_long\"}}", line.at);
        }
        text.push('\n');
        match self.append(text.as_bytes()) {
            Ok(()) => {
                self.written.fetch_add(1, Ordering::Relaxed);
                self.healthy.store(true, Ordering::Relaxed);
            }
            Err(e) => self.note_failure(e.kind()),
        }
    }

    /// The file is the record; nothing is kept in memory.
    fn records(&self) -> Vec<DenialRecord> {
        Vec::new()
    }
}

// ---- the counting sink -----------------------------------------------------------------------

/// Counts every denial per purpose and on `spot_egress_denied_total{purpose}`, then hands it on.
/// It sits outside the rate limit, so the counts are complete even when the file drops lines.
pub struct CountingSink {
    inner: Arc<dyn DenialSink>,
    counts: Vec<AtomicU64>,
}

impl CountingSink {
    /// A counting sink in front of `inner`.
    pub fn new(inner: Arc<dyn DenialSink>) -> Self {
        // One slot per known purpose and one for a purpose a newer guard may add.
        let counts = (0..=PURPOSES.len()).map(|_| AtomicU64::new(0)).collect();
        Self { inner, counts }
    }

    /// Denials since start, per purpose (every purpose listed, zero included).
    pub fn by_purpose(&self) -> BTreeMap<&'static str, u64> {
        let mut out = BTreeMap::new();
        for (i, name) in PURPOSES.iter().enumerate() {
            out.insert(*name, self.counts[i].load(Ordering::Relaxed));
        }
        out
    }

    /// All denials since start.
    pub fn total(&self) -> u64 {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }
}

impl DenialSink for CountingSink {
    fn denied(&self, record: DenialRecord) {
        let purpose = record.purpose.as_str();
        let slot = PURPOSES
            .iter()
            .position(|p| *p == purpose)
            .unwrap_or(PURPOSES.len());
        self.counts[slot].fetch_add(1, Ordering::Relaxed);
        metrics::counter!(METRIC_DENIED, "purpose" => purpose).increment(1);
        self.inner.denied(record);
    }

    fn records(&self) -> Vec<DenialRecord> {
        self.inner.records()
    }
}

// ---- reading ---------------------------------------------------------------------------------

/// What `spot egress denials` asks for.
#[derive(Clone, Debug)]
pub struct DenialQuery {
    /// Only denials at or after this unix second.
    pub since: Option<i64>,
    /// Only this purpose.
    pub purpose: Option<String>,
    /// Only this tenant.
    pub tenant: Option<String>,
    /// At most this many, newest first (capped at [`LIMIT_MAX`]).
    pub limit: usize,
}

fn tail(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let start = len.saturating_sub(READ_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(READ_TAIL_BYTES).read_to_end(&mut bytes)?;
    if start > 0 {
        // The cut is mid-line: drop the partial first line.
        let cut = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        bytes.drain(..cut);
    }
    Ok(bytes)
}

/// The denials in the file at `path` (and the first rotated file), newest first, filtered.
///
/// Reads at most [`READ_TAIL_BYTES`] from each of the two files; a line that does not parse is
/// skipped. A missing file is an empty answer.
///
/// # Errors
///
/// The file exists but cannot be read.
pub fn read_denials(path: &Path, query: &DenialQuery) -> std::io::Result<Vec<DenialLine>> {
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    let mut lines: Vec<DenialLine> = Vec::new();
    for file in [PathBuf::from(rotated), path.to_path_buf()] {
        let bytes = tail(&file)?;
        for raw in bytes.split(|b| *b == b'\n') {
            if raw.is_empty() {
                continue;
            }
            if let Ok(line) = serde_json::from_slice::<DenialLine>(raw) {
                lines.push(line);
            }
        }
    }
    let mut out: VecDeque<DenialLine> = VecDeque::new();
    let limit = query.limit.clamp(1, LIMIT_MAX);
    for line in lines.into_iter().rev() {
        if query.since.is_some_and(|s| line.at < s) {
            continue;
        }
        if query.purpose.as_deref().is_some_and(|p| line.purpose != p) {
            continue;
        }
        if query.tenant.as_deref().is_some_and(|t| line.tenant != t) {
            continue;
        }
        out.push_back(line);
        if out.len() >= limit {
            break;
        }
    }
    Ok(out.into())
}

/// `30m`, `1h`, `7d`, `90s` as seconds. At least one second, at most 3650 days.
///
/// # Errors
///
/// A text that is not a number followed by `s`, `m`, `h` or `d`.
pub fn parse_since(text: &str) -> Result<i64, String> {
    let bad = || {
        format!(
            "`{}` is not a duration such as 30m, 1h or 7d",
            text.escape_debug()
        )
    };
    let unit = text.chars().last().ok_or_else(bad)?;
    let digits = &text[..text.len() - unit.len_utf8()];
    let n: i64 = digits.parse().map_err(|_| bad())?;
    let per = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(bad()),
    };
    let seconds = n.checked_mul(per).ok_or_else(bad)?;
    if !(1..=3650 * 86_400).contains(&seconds) {
        return Err(bad());
    }
    Ok(seconds)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn since_parses_the_four_units_and_refuses_the_rest() {
        assert_eq!(parse_since("90s").unwrap(), 90);
        assert_eq!(parse_since("30m").unwrap(), 1800);
        assert_eq!(parse_since("1h").unwrap(), 3600);
        assert_eq!(parse_since("7d").unwrap(), 604_800);
        for bad in ["", "h", "0h", "-1h", "1w", "1.5h", "99999999d"] {
            assert!(parse_since(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn class_names_are_snake_case() {
        assert_eq!(class_name(AddrClass::CarrierGradeNat), "carrier_grade_nat");
        assert_eq!(class_name(AddrClass::Loopback), "loopback");
    }
}
