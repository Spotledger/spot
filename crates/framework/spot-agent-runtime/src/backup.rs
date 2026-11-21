//! Backups: a base backup or a logical dump, list and verify.
//!
//! The data movement itself is `spot-snapshot`'s job (its providers take the dump or the
//! copy and write a canonical manifest); this module owns the management operation around it:
//! the typed request, the listing the console shows, and the integrity check. Verification
//! returns [`spot_snapshot::VerifyReport`] so the numbers the console displays are the
//! snapshot crate's own file and byte counts, never a second implementation.
//!
//! All machine and filesystem work goes through [`BackupPort`]; the module reads no clock.

use spot_snapshot::VerifyReport;

use crate::error::{RuntimeError, RuntimeResult};

/// What kind of backup to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupKind {
    /// A physical `pg_basebackup` for recovery and standby creation.
    BaseBackup,
    /// A logical dump for a portable, selective restore.
    LogicalDump,
}

impl BackupKind {
    /// The lower-case wire name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::BaseBackup => "base",
            Self::LogicalDump => "logical",
        }
    }

    /// The kind named `name`, if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "base" => Some(Self::BaseBackup),
            "logical" => Some(Self::LogicalDump),
            _ => None,
        }
    }
}

/// A request to start a backup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupRequest {
    /// Physical or logical.
    pub kind: BackupKind,
    /// A short label the operator recognises (`pre-upgrade`, `nightly`).
    pub label: String,
}

/// What starting a backup produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupStarted {
    /// The provider's snapshot id.
    pub id: String,
    /// Where it was written (the manifest's location; never a secret).
    pub location: String,
    /// Physical or logical.
    pub kind: BackupKind,
    /// Unix microseconds the take finished at.
    pub created_at: i64,
}

/// One backup the node holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupEntry {
    /// The provider's snapshot id.
    pub id: String,
    /// Physical or logical.
    pub kind: BackupKind,
    /// The operator's label.
    pub label: String,
    /// Size on disk, bytes, integer.
    pub bytes: u64,
    /// Unix microseconds the take finished at.
    pub created_at: i64,
    /// Whether the last verification passed.
    pub verified: bool,
}

/// The result of verifying one backup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupVerification {
    /// The backup's id.
    pub id: String,
    /// Files the provider hashed.
    pub files_checked: u64,
    /// Bytes the provider hashed.
    pub bytes_checked: u64,
}

/// The backup seam. The real adapter wraps `spot-snapshot`'s provider and its manifest;
/// tests script it.
pub trait BackupPort: Send + Sync {
    /// Starts a backup.
    ///
    /// # Errors
    /// A backend error when the take cannot start or finish.
    fn start(&self, request: &BackupRequest, now: i64) -> RuntimeResult<BackupStarted>;

    /// Lists the backups this node holds, newest first.
    ///
    /// # Errors
    /// A backend error when the directory cannot be read.
    fn list(&self) -> RuntimeResult<Vec<BackupEntry>>;

    /// Verifies one backup's manifest and file checksums.
    ///
    /// # Errors
    /// A backend error when the backup cannot be read or a checksum fails.
    fn verify(&self, id: &str) -> RuntimeResult<VerifyReport>;
}

/// Starts a backup after validating the typed request.
///
/// # Errors
/// [`RuntimeError::InvalidArgument`] for an empty or unsafe label, and the port's error.
pub fn start_backup(
    backup: &dyn BackupPort,
    request: &BackupRequest,
    now: i64,
) -> RuntimeResult<BackupStarted> {
    let label = request.label.trim();
    if label.is_empty() {
        return Err(invalid("start", "label", "must not be empty"));
    }
    if label.len() > 64 {
        return Err(invalid("start", "label", "must be at most 64 bytes"));
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(invalid(
            "start",
            "label",
            "may contain only letters, digits, dash, underscore and dot",
        ));
    }
    let validated = BackupRequest {
        kind: request.kind,
        label: label.to_owned(),
    };
    backup.start(&validated, now)
}

/// Lists the backups, newest first.
///
/// # Errors
/// The port's error.
pub fn list_backups(backup: &dyn BackupPort) -> RuntimeResult<Vec<BackupEntry>> {
    let mut entries = backup.list()?;
    entries.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(entries)
}

/// Verifies one backup and maps the provider's report.
///
/// # Errors
/// [`RuntimeError::InvalidArgument`] for an empty id, [`RuntimeError::BackupVerifyFailed`]
/// when the provider reports a failure, or the port's error.
pub fn verify_backup(backup: &dyn BackupPort, id: &str) -> RuntimeResult<BackupVerification> {
    if id.is_empty()
        || id.len() > 128
        || id == "."
        || id.contains("..")
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(invalid(
            "verify",
            "id",
            "must be a backup id of letters, digits, dash, underscore and dot",
        ));
    }
    match backup.verify(id) {
        Ok(report) => Ok(BackupVerification {
            id: id.to_owned(),
            files_checked: report.files_checked,
            bytes_checked: report.bytes_checked,
        }),
        Err(error) => Err(RuntimeError::BackupVerifyFailed {
            id: id.to_owned(),
            detail: error.to_string(),
        }),
    }
}

/// The invalid-argument error for a backup operation.
fn invalid(op: &str, param: &str, detail: &str) -> RuntimeError {
    RuntimeError::InvalidArgument {
        op: format!("db.backup.{op}"),
        param: param.to_owned(),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeBackup {
        entries: Mutex<Vec<BackupEntry>>,
        starts: Mutex<Vec<BackupRequest>>,
        fail_verify: bool,
    }

    impl FakeBackup {
        fn with_entries(entries: Vec<BackupEntry>) -> Self {
            Self {
                entries: Mutex::new(entries),
                ..Self::default()
            }
        }
    }

    impl BackupPort for FakeBackup {
        fn start(&self, request: &BackupRequest, now: i64) -> RuntimeResult<BackupStarted> {
            self.starts.lock().unwrap().push(request.clone());
            Ok(BackupStarted {
                id: format!("snap-{}", now),
                location: "/var/backups/spot".to_owned(),
                kind: request.kind,
                created_at: now,
            })
        }

        fn list(&self) -> RuntimeResult<Vec<BackupEntry>> {
            Ok(self.entries.lock().unwrap().clone())
        }

        fn verify(&self, _id: &str) -> RuntimeResult<VerifyReport> {
            if self.fail_verify {
                return Err(RuntimeError::BackupFailed {
                    detail: "checksum mismatch".to_owned(),
                });
            }
            Ok(VerifyReport {
                files_checked: 3,
                bytes_checked: 4096,
            })
        }
    }

    fn entry(id: &str, created_at: i64) -> BackupEntry {
        BackupEntry {
            id: id.to_owned(),
            kind: BackupKind::BaseBackup,
            label: "nightly".to_owned(),
            bytes: 1024,
            created_at,
            verified: false,
        }
    }

    #[test]
    fn a_backup_request_is_validated() {
        let backup = FakeBackup::default();
        let ok = BackupRequest {
            kind: BackupKind::LogicalDump,
            label: "pre-upgrade".to_owned(),
        };
        assert_eq!(start_backup(&backup, &ok, 7).unwrap().id, "snap-7");
        assert_eq!(backup.starts.lock().unwrap().len(), 1);

        for bad in ["", "has space", "sl/ash", &"x".repeat(65)] {
            let request = BackupRequest {
                kind: BackupKind::BaseBackup,
                label: bad.to_owned(),
            };
            assert_eq!(
                start_backup(&backup, &request, 7).unwrap_err().code(),
                crate::error::INVALID_ARGUMENT
            );
        }
    }

    #[test]
    fn listing_is_newest_first() {
        let backup = FakeBackup::with_entries(vec![entry("a", 1), entry("c", 3), entry("b", 2)]);
        let ids: Vec<String> = list_backups(&backup)
            .unwrap()
            .into_iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(ids, vec!["c", "b", "a"]);
    }

    #[test]
    fn verification_returns_the_provider_report() {
        let backup = FakeBackup::default();
        let report = verify_backup(&backup, "snap-1").unwrap();
        assert_eq!(report.files_checked, 3);
        assert_eq!(report.bytes_checked, 4096);

        let failing = FakeBackup {
            fail_verify: true,
            ..FakeBackup::default()
        };
        assert_eq!(
            verify_backup(&failing, "snap-1").unwrap_err().code(),
            crate::error::BACKUP_VERIFY_FAILED
        );
        assert!(verify_backup(&backup, " ").is_err());
        assert!(verify_backup(&backup, "../../etc/passwd").is_err());
        assert!(verify_backup(&backup, "a/b").is_err());
    }

    #[test]
    fn kinds_round_trip() {
        assert_eq!(BackupKind::from_name("base"), Some(BackupKind::BaseBackup));
        assert_eq!(
            BackupKind::from_name("logical"),
            Some(BackupKind::LogicalDump)
        );
        assert_eq!(BackupKind::from_name("partial"), None);
    }
}
