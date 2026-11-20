//! The local command audit log: an append-only file of every command decision
//! (accepted, refused with reason, executed, result code), one JSON line per entry,
//! hash-chained with the `spot-audit-chain` entry (`SHA-256(seq, prev, record)`), fsynced
//! per append and rotated by size. It never holds parameter values: a
//! [`DecisionRecord`] carries the command digest, the operation id, the signers and the
//! outcome only.
//!
//! The log implements [`ResultLog`], so it plugs straight into `spot_agent_core::Agent`
//! and `Executor`; a failure to record refuses the command (fail closed).

use std::collections::BTreeMap;
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use spot_agent_core::{DecisionRecord, Refusal, ResultLog};
use spot_audit_chain::{AuditRecord, ChainEntry, Hash, PayloadValue, genesis_hash};

use crate::error::AgentError;

/// Default rotation threshold: 4 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Rotated files kept.
pub const DEFAULT_KEEP: usize = 8;

struct Head {
    seq: u64,
    prev: Hash,
    size: u64,
}

/// The file-backed hash-chained log.
pub struct LocalAuditLog {
    site: String,
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    head: Mutex<Head>,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Renders a record as an audit-chain record, exactly as `spot-agent-core` does for its
/// own result logs (so a verifier sees one shape).
fn to_audit_record(r: &DecisionRecord) -> Result<AuditRecord, Refusal> {
    let at = r.at.checked_mul(1_000_000).ok_or(Refusal::LogUnavailable)?;
    let mut payload: BTreeMap<String, PayloadValue> = BTreeMap::new();
    payload.insert(
        "command_id".to_owned(),
        PayloadValue::Text(r.command_id.clone()),
    );
    payload.insert(
        "command_digest".to_owned(),
        PayloadValue::Text(hex(&r.command_digest)),
    );
    payload.insert("op".to_owned(), PayloadValue::Text(r.op.clone()));
    payload.insert("outcome".to_owned(), PayloadValue::Text(r.outcome.clone()));
    payload.insert(
        "signers".to_owned(),
        PayloadValue::Text(r.signers.join(",")),
    );
    if let Some(code) = &r.code {
        payload.insert("code".to_owned(), PayloadValue::Text(code.clone()));
    }
    Ok(AuditRecord {
        kind: format!("agent_core.{}", r.outcome),
        actor: r.actor.clone(),
        on_behalf_of: None,
        subject: None,
        at,
        payload,
        masked: true,
    })
}

fn last_entry(path: &Path) -> Option<ChainEntry> {
    let file = std::fs::File::open(path).ok()?;
    let mut last = None;
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if !line.trim().is_empty() {
            last = Some(line);
        }
    }
    serde_json::from_str(&last?).ok()
}

impl LocalAuditLog {
    /// Opens (creating on first use) the log at `path` for `site`, continuing the chain.
    ///
    /// # Errors
    /// [`AgentError::AuditFailed`] when the existing tail is unreadable.
    pub fn open(path: impl Into<PathBuf>, site: impl Into<String>) -> Result<Self, AgentError> {
        Self::open_with(path, site, DEFAULT_MAX_BYTES, DEFAULT_KEEP)
    }

    /// As [`Self::open`] with explicit rotation settings.
    ///
    /// # Errors
    /// [`AgentError::AuditFailed`].
    pub fn open_with(
        path: impl Into<PathBuf>,
        site: impl Into<String>,
        max_bytes: u64,
        keep: usize,
    ) -> Result<Self, AgentError> {
        let path = path.into();
        let site = site.into();
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        let tail = if size > 0 {
            Some(last_entry(&path).ok_or_else(|| {
                AgentError::AuditFailed("the audit log tail is unreadable".to_owned())
            })?)
        } else {
            Self::rotated_files(&path)
                .last()
                .and_then(|(_, p)| last_entry(p))
        };
        let head = match tail {
            Some(e) => Head {
                seq: e.seq,
                prev: e.hash,
                size,
            },
            None => Head {
                seq: 0,
                prev: genesis_hash(&site),
                size,
            },
        };
        Ok(Self {
            site,
            path,
            max_bytes,
            keep,
            head: Mutex::new(head),
        })
    }

    /// The rotated files of `path`, oldest first, with their numeric suffix.
    fn rotated_files(path: &Path) -> Vec<(u64, PathBuf)> {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Vec::new();
        };
        let prefix = format!("{}.", name.to_string_lossy());
        let mut out: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let file = e.file_name().to_string_lossy().into_owned();
                let n: u64 = file.strip_prefix(&prefix)?.parse().ok()?;
                Some((n, e.path()))
            })
            .collect();
        out.sort();
        out
    }

    fn rotate(&self, head: &Head) -> std::io::Result<()> {
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{:020}", head.seq));
        std::fs::rename(&self.path, PathBuf::from(name))?;
        let rotated = Self::rotated_files(&self.path);
        if rotated.len() > self.keep {
            for (_, old) in &rotated[..rotated.len() - self.keep] {
                let _ = std::fs::remove_file(old);
            }
        }
        Ok(())
    }

    /// The number of entries appended so far (the chain head sequence).
    #[must_use]
    pub fn head_seq(&self) -> u64 {
        self.head.lock().map_or(0, |h| h.seq)
    }
}

impl ResultLog for LocalAuditLog {
    fn site(&self) -> &str {
        &self.site
    }

    fn append(&self, record: &DecisionRecord) -> Result<ChainEntry, Refusal> {
        let audit = to_audit_record(record)?;
        audit.validate().map_err(|_| Refusal::LogUnavailable)?;
        let mut head = self.head.lock().map_err(|_| Refusal::LogUnavailable)?;
        let entry = ChainEntry::new(head.seq.saturating_add(1), head.prev, audit);
        let mut line = serde_json::to_string(&entry).map_err(|_| Refusal::LogUnavailable)?;
        line.push('\n');
        let len = u64::try_from(line.len()).map_err(|_| Refusal::LogUnavailable)?;
        if head.size > 0 && head.size.saturating_add(len) > self.max_bytes {
            self.rotate(&head).map_err(|_| Refusal::LogUnavailable)?;
            head.size = 0;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.path)
            .map_err(|_| Refusal::LogUnavailable)?;
        file.write_all(line.as_bytes())
            .map_err(|_| Refusal::LogUnavailable)?;
        file.sync_all().map_err(|_| Refusal::LogUnavailable)?;
        head.seq = entry.seq;
        head.prev = entry.hash;
        head.size = head.size.saturating_add(len);
        Ok(entry)
    }
}

/// Verifies one audit file: sequence without gaps, each `prev_hash` equal to the previous
/// entry's hash, every hash recomputed. The first entry is the anchor (genesis of `site`
/// when its sequence is 1). Returns the entry count.
///
/// # Errors
/// [`AgentError::AuditFailed`] naming the first bad line (1-based), never its content.
pub fn verify_file(path: &Path, site: &str) -> Result<u64, AgentError> {
    let fail = |n: u64| AgentError::AuditFailed(format!("audit chain broken at line {n}"));
    let file = std::fs::File::open(path)
        .map_err(|_| AgentError::AuditFailed("cannot open the audit log".to_owned()))?;
    let mut prev: Option<(u64, Hash)> = None;
    let mut count = 0u64;
    let mut last_line = 0u64;
    for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
        let n = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        let line = line.map_err(|_| fail(n))?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: ChainEntry = serde_json::from_str(&line).map_err(|_| fail(n))?;
        if entry.recompute_hash() != entry.hash {
            return Err(fail(n));
        }
        match prev {
            None if entry.seq == 1 && entry.prev_hash != genesis_hash(site) => return Err(fail(n)),
            None => {}
            Some((seq, hash)) if entry.seq != seq.saturating_add(1) || entry.prev_hash != hash => {
                return Err(fail(n));
            }
            Some(_) => {}
        }
        prev = Some((entry.seq, entry.hash));
        count += 1;
        last_line = n;
    }
    // Every entry is written with its newline: a file that ends without one lost its tail
    // (a cut inside the last entry that still parses is caught here, a cut inside its text
    // by the parse above).
    if last_line > 0 && !ends_with_newline(path) {
        return Err(fail(last_line));
    }
    Ok(count)
}

fn ends_with_newline(path: &Path) -> bool {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut byte = [0u8; 1];
    file.seek(SeekFrom::End(-1)).is_ok() && file.read_exact(&mut byte).is_ok() && byte[0] == b'\n'
}

/// A local-event record (enrolment, start, stop): the same chain, no command.
#[must_use]
pub fn event_record(
    agent: &str,
    op: &str,
    outcome: &str,
    code: Option<&str>,
    at: i64,
) -> DecisionRecord {
    DecisionRecord {
        actor: format!("agent:{agent}"),
        command_id: String::new(),
        command_digest: [0u8; 32],
        op: op.to_owned(),
        signers: Vec::new(),
        outcome: outcome.to_owned(),
        code: code.map(str::to_owned),
        at,
    }
}
