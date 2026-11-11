//! The update journal: the one file a crash is resumed from.
//!
//! The engine writes the journal **before and after** every step, durably, so at any instant the
//! file says which step was in progress and what was true when it started. After the power comes
//! back, [`Journal::load`] recovers that state and [`crate::engine::UpdateEngine::resume`] brings
//! the install to one of exactly two states: the old version running and intact, or the new
//! version running and committed.
//!
//! The file is a checksum line, then the canonical JSON body. The checksum is verified on read
//! (ADR-068 rule 22): a journal corrupted by a torn write is refused, never read as data.

use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::UpdaterError;
use crate::fs::FileSystem;

/// The journal format marker; a journal written by another format is refused.
pub const JOURNAL_SCHEMA: u32 = 1;

/// The update steps, in the order they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// Read the offered manifest, signature and payload.
    Receive,
    /// Verify the signature, the payload digest and the per-file hashes (part 1).
    Verify,
    /// Unpack the payload into the staging directory.
    Unpack,
    /// Re-read and re-hash every staged file.
    VerifyOnDisk,
    /// Stop the running service.
    Quiesce,
    /// Take the pre-migration backup.
    Backup,
    /// Migrate the data.
    Migrate,
    /// Point `current` at the new version.
    Switch,
    /// Start the new version.
    StartNew,
    /// Wait for health within the deadline.
    Health,
    /// Record the commit and prune old versions.
    Commit,
}

impl Step {
    /// The step's stable name, as the journal stores it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Receive => "receive",
            Self::Verify => "verify",
            Self::Unpack => "unpack",
            Self::VerifyOnDisk => "verify_on_disk",
            Self::Quiesce => "quiesce",
            Self::Backup => "backup",
            Self::Migrate => "migrate",
            Self::Switch => "switch",
            Self::StartNew => "start_new",
            Self::Health => "health",
            Self::Commit => "commit",
        }
    }

    /// Parses a step name.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "receive" => Self::Receive,
            "verify" => Self::Verify,
            "unpack" => Self::Unpack,
            "verify_on_disk" => Self::VerifyOnDisk,
            "quiesce" => Self::Quiesce,
            "backup" => Self::Backup,
            "migrate" => Self::Migrate,
            "switch" => Self::Switch,
            "start_new" => Self::StartNew,
            "health" => Self::Health,
            "commit" => Self::Commit,
            _ => return None,
        })
    }
}

/// What the journal says the update ended as, if it has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// The step named by [`Journal::step`] was in progress (or finished and moved on).
    InProgress,
    /// The new version is committed and serving.
    Committed,
    /// The update rolled back; the previous version serves.
    RolledBack,
    /// Neither the update nor its rollback could complete; the journal says what is where.
    NeedsAttention,
}

impl Stage {
    /// The stage's stable name, as the journal stores it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Committed => "committed",
            Self::RolledBack => "rolled_back",
            Self::NeedsAttention => "needs_attention",
        }
    }

    /// Parses a stage name.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "in_progress" => Self::InProgress,
            "committed" => Self::Committed,
            "rolled_back" => Self::RolledBack,
            "needs_attention" => Self::NeedsAttention,
            _ => return None,
        })
    }
}

/// The engine policy that must survive a restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPolicy {
    /// Post-commit probation in seconds.
    pub probation_secs: u64,
    /// Number of recent versions to retain, in addition to protected pointers.
    pub retention: usize,
    /// Readiness timeout in seconds.
    pub health_timeout_secs: i64,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self::from(&crate::UpdatePolicy::default())
    }
}

impl From<&crate::UpdatePolicy> for RecoveryPolicy {
    fn from(policy: &crate::UpdatePolicy) -> Self {
        Self {
            probation_secs: policy.probation_secs,
            retention: policy.retention,
            health_timeout_secs: policy.health_timeout_secs,
        }
    }
}

impl RecoveryPolicy {
    /// Reconstructs the recovery fields; decision-only fields are not used during recovery.
    #[must_use]
    pub fn policy(&self) -> crate::UpdatePolicy {
        crate::UpdatePolicy {
            probation_secs: self.probation_secs,
            retention: self.retention,
            health_timeout_secs: self.health_timeout_secs,
            ..crate::UpdatePolicy::default()
        }
    }
}

/// The journal's typed contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Journal {
    /// The policy captured before mutation; recovery must not substitute today's defaults.
    pub recovery_policy: RecoveryPolicy,
    /// Durable rollback intent, including when rolling back a committed probation run.
    pub rollback_started: bool,
    /// The backup has been restored; do not restore it again after restarting old code.
    pub restore_done: bool,
    /// Whether the current step completed (written after its action).
    pub step_finished: bool,
    /// The update format marker.
    pub schema: u32,
    /// The version being applied.
    pub target: Version,
    /// The version `current` named before the switch, if any.
    pub from: Option<Version>,
    /// The version `previous` named before the run, so a rollback restores it unchanged.
    pub previous_before: Option<Version>,
    /// What the update has ended as, if it has ended.
    pub stage: Stage,
    /// The step that was last started.
    pub step: Step,
    /// Whether the service has been stopped (so a rollback must restart it).
    pub quiesced: bool,
    /// Whether the migrate hook was started (so a rollback must restore).
    pub migrate_started: bool,
    /// When the commit was recorded (Unix seconds), for the probation window.
    pub committed_at: Option<i64>,
    /// A human-readable note when the stage is [`Stage::NeedsAttention`].
    pub note: Option<String>,
}

/// The JSON shape, with versions as strings so the journal needs no serde feature on `semver`.
#[derive(Serialize, Deserialize)]
struct JournalFile {
    #[serde(default)]
    recovery_policy: RecoveryPolicy,
    #[serde(default)]
    rollback_started: bool,
    #[serde(default)]
    restore_done: bool,
    #[serde(default)]
    step_finished: bool,
    schema: u32,
    target: String,
    from: Option<String>,
    previous_before: Option<String>,
    stage: String,
    step: String,
    quiesced: bool,
    migrate_started: bool,
    committed_at: Option<i64>,
    note: Option<String>,
}

impl Journal {
    /// A new journal for applying `target`, remembering `from` as the version to fall back to
    /// and `previous_before` as the last-good pointer to restore on a rollback.
    #[must_use]
    pub fn new(target: Version, from: Option<Version>, previous_before: Option<Version>) -> Self {
        Self {
            recovery_policy: RecoveryPolicy::default(),
            rollback_started: false,
            restore_done: false,
            step_finished: false,
            schema: JOURNAL_SCHEMA,
            target,
            from,
            previous_before,
            stage: Stage::InProgress,
            step: Step::Receive,
            quiesced: false,
            migrate_started: false,
            committed_at: None,
            note: None,
        }
    }

    /// The file's bytes: the checksum line, then the canonical JSON body.
    ///
    /// # Errors
    /// [`UpdaterError::Journal`] when the body cannot be serialised.
    pub fn to_bytes(&self) -> Result<Vec<u8>, UpdaterError> {
        let file = JournalFile {
            recovery_policy: self.recovery_policy.clone(),
            rollback_started: self.rollback_started,
            restore_done: self.restore_done,
            step_finished: self.step_finished,
            schema: self.schema,
            target: self.target.to_string(),
            from: self.from.as_ref().map(ToString::to_string),
            previous_before: self.previous_before.as_ref().map(ToString::to_string),
            stage: self.stage.as_str().to_owned(),
            step: self.step.as_str().to_owned(),
            quiesced: self.quiesced,
            migrate_started: self.migrate_started,
            committed_at: self.committed_at,
            note: self.note.clone(),
        };
        let body = serde_json::to_vec(&file).map_err(|error| UpdaterError::Journal {
            detail: error.to_string(),
        })?;
        let mut out = Digest::of(&body).to_hex().into_bytes();
        out.push(b'\n');
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Parses the file's bytes, verifying the checksum first.
    ///
    /// # Errors
    /// [`UpdaterError::Journal`] when the bytes are not UTF-8, the checksum line is missing or
    /// does not match, the JSON does not parse, or a version or enumerated name is unknown.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, UpdaterError> {
        let text = std::str::from_utf8(bytes).map_err(|error| UpdaterError::Journal {
            detail: format!("the journal is not UTF-8: {error}"),
        })?;
        let (checksum, body) = text.split_once('\n').ok_or_else(|| UpdaterError::Journal {
            detail: "the journal has no checksum line".to_owned(),
        })?;
        if checksum != Digest::of(body.as_bytes()).to_hex() {
            return Err(UpdaterError::Journal {
                detail: "the journal checksum does not match its body".to_owned(),
            });
        }
        let file: JournalFile =
            serde_json::from_str(body).map_err(|error| UpdaterError::Journal {
                detail: error.to_string(),
            })?;
        if file.schema != JOURNAL_SCHEMA {
            return Err(UpdaterError::Journal {
                detail: format!(
                    "the journal schema is {}, not {JOURNAL_SCHEMA}",
                    file.schema
                ),
            });
        }
        let step = Step::parse(&file.step).ok_or_else(|| UpdaterError::Journal {
            detail: format!("unknown step `{}`", file.step),
        })?;
        let stage = Stage::parse(&file.stage).ok_or_else(|| UpdaterError::Journal {
            detail: format!("unknown stage `{}`", file.stage),
        })?;
        Ok(Self {
            recovery_policy: file.recovery_policy,
            rollback_started: file.rollback_started,
            restore_done: file.restore_done,
            step_finished: file.step_finished,
            schema: file.schema,
            target: version_of(&file.target)?,
            from: match file.from {
                Some(text) => Some(version_of(&text)?),
                None => None,
            },
            previous_before: match file.previous_before {
                Some(text) => Some(version_of(&text)?),
                None => None,
            },
            stage,
            step,
            quiesced: file.quiesced,
            migrate_started: file.migrate_started,
            committed_at: file.committed_at,
            note: file.note,
        })
    }

    /// Loads the journal at `path`, `None` when it does not exist.
    ///
    /// # Errors
    /// [`UpdaterError::Io`] on a read failure, [`UpdaterError::Journal`] on a bad file.
    pub fn load(fs: &dyn FileSystem, path: &Path) -> Result<Option<Self>, UpdaterError> {
        if !fs.exists(path)? {
            return Ok(None);
        }
        let bytes = fs.read(path)?;
        Ok(Some(Self::from_bytes(&bytes)?))
    }

    /// Writes the journal durably.
    ///
    /// # Errors
    /// [`UpdaterError::Journal`] on a serialisation failure, or the write's own error.
    pub fn store(&self, fs: &dyn FileSystem, path: &Path) -> Result<(), UpdaterError> {
        fs.durable_write(path, &self.to_bytes()?)
    }

    /// Removes the journal, ignoring a missing file.
    ///
    /// # Errors
    /// [`UpdaterError::Io`] when it exists and cannot be removed.
    pub fn remove(fs: &dyn FileSystem, path: &Path) -> Result<(), UpdaterError> {
        fs.remove_file(path)
    }
}

/// Parses a version out of the journal.
fn version_of(text: &str) -> Result<Version, UpdaterError> {
    Version::parse(text).map_err(|error| UpdaterError::Journal {
        detail: format!("`{text}` is not a version: {error}"),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_journal_round_trips_and_checks_its_checksum() {
        let journal = Journal::new(
            Version::parse("2.0.0").unwrap(),
            Some(Version::parse("1.9.0").unwrap()),
            None,
        );
        let bytes = journal.to_bytes().unwrap();
        assert_eq!(Journal::from_bytes(&bytes).unwrap(), journal);

        let mut tampered = bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert_eq!(
            Journal::from_bytes(&tampered).unwrap_err().code().as_str(),
            "updater.journal"
        );
    }

    #[test]
    fn every_step_and_stage_name_round_trips() {
        for step in [
            Step::Receive,
            Step::Verify,
            Step::Unpack,
            Step::VerifyOnDisk,
            Step::Quiesce,
            Step::Backup,
            Step::Migrate,
            Step::Switch,
            Step::StartNew,
            Step::Health,
            Step::Commit,
        ] {
            assert_eq!(Step::parse(step.as_str()), Some(step));
        }
        for stage in [
            Stage::InProgress,
            Stage::Committed,
            Stage::RolledBack,
            Stage::NeedsAttention,
        ] {
            assert_eq!(Stage::parse(stage.as_str()), Some(stage));
        }
    }
}
