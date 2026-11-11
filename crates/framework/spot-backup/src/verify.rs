//! Backup verification: `Quick` (digest, size, header) and `Full` (scratch restore +
//! schema checks + L7 invariants).
//!
//! A report says what was checked and, on failure, the first reason. The runner records
//! a backup as the last good one only when `ok` is true; an unverified package is never
//! restorable.

use crate::error::{BackupError, Result};
use crate::model::VerifyMode;
use crate::package::PackageFile;
use crate::site::{InvariantReport, InvariantRunner, RestoredSite, ScratchSite};

/// The result of checking one package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// The requested mode.
    pub mode: VerifyMode,
    /// Whether every check passed.
    pub ok: bool,
    /// The checks that ran, in order.
    pub checks: Vec<String>,
    /// The first failure's reason, when `ok` is false.
    pub failure: Option<String>,
    /// Whole package bytes, integer.
    pub package_bytes: u64,
    /// Payload bytes, integer.
    pub payload_bytes: u64,
    /// Payload files, integer.
    pub file_count: u64,
    /// The package digest.
    pub digest: String,
    /// The scratch site's invariant report, for `Full`.
    pub invariants: Option<InvariantReport>,
}

impl VerifyReport {
    /// A report that passed both quick and full checks.
    pub fn ok(mode: VerifyMode, package: &PackageFile, checks: Vec<String>) -> VerifyReport {
        VerifyReport {
            mode,
            ok: true,
            checks,
            failure: None,
            package_bytes: package.package_bytes(),
            payload_bytes: package.manifest().size,
            file_count: package.manifest().file_count,
            digest: package.manifest().digest.clone(),
            invariants: None,
        }
    }

    /// A failed report with one reason.
    pub fn failed(mode: VerifyMode, package: &PackageFile, reason: String) -> VerifyReport {
        VerifyReport {
            mode,
            ok: false,
            checks: Vec::new(),
            failure: Some(reason),
            package_bytes: package.package_bytes(),
            payload_bytes: package.manifest().size,
            file_count: package.manifest().file_count,
            digest: package.manifest().digest.clone(),
            invariants: None,
        }
    }
}

/// The quick verification: manifest digest, payload size, file count and header.
///
/// # Errors
/// Never returns `Err`: the failure is in [`VerifyReport::failure`] so a runner can
/// always record the failed attempt.
pub fn quick(package: &PackageFile) -> VerifyReport {
    let mut report = VerifyReport::ok(
        VerifyMode::Quick,
        package,
        vec![
            "header".to_owned(),
            "digest".to_owned(),
            "size".to_owned(),
            "file_count".to_owned(),
        ],
    );
    match crate::package::verify_quick(package) {
        Ok(()) => report,
        Err(error) => {
            report.ok = false;
            report.failure = Some(error.reason());
            report
        }
    }
}

/// The full verification: a scratch restore, schema checks, then the invariant runner.
///
/// # Errors
/// [`BackupError`] when the scratch restore itself fails; an invariant violation is a
/// report with `ok = false`, not an error, so the caller can alert and keep the gate
/// closed.
pub async fn full(
    package: &PackageFile,
    scratch: &dyn ScratchSite,
    invariants: &dyn InvariantRunner,
) -> Result<VerifyReport> {
    let quick_report = quick(package);
    if !quick_report.ok {
        return Ok(quick_report);
    }
    let mut report = VerifyReport::ok(
        VerifyMode::Full,
        package,
        vec![
            "header".to_owned(),
            "digest".to_owned(),
            "size".to_owned(),
            "file_count".to_owned(),
            "scratch_restore".to_owned(),
        ],
    );
    let site: Box<dyn RestoredSite> = scratch.restore(package).await?;
    if site.tenant() != package.manifest().tenant {
        report.ok = false;
        report.failure = Some(format!(
            "the scratch restore belongs to tenant {} but the backup is for {}",
            site.tenant(),
            package.manifest().tenant
        ));
        return Ok(report);
    }
    let checks = site.schema_checks()?;
    report.checks.push("schema_checks".to_owned());
    let failed: Vec<String> = checks
        .iter()
        .filter(|check| !check.passed)
        .map(|check| format!("{}: {}", check.name, check.detail))
        .collect();
    if !failed.is_empty() {
        report.ok = false;
        report.failure = Some(failed.join("; "));
        return Ok(report);
    }
    let invariant_report = invariants.run(site.as_ref()).await?;
    report.checks.push("l7_invariants".to_owned());
    report.ok = invariant_report.ok;
    if !invariant_report.ok {
        report.failure = Some(invariant_report.violations.join("; "));
    }
    report.invariants = Some(invariant_report);
    Ok(report)
}

/// The health condition a failed report raises.
pub fn condition_for(report: &VerifyReport, backup_id: &str) -> crate::site::HealthCondition {
    let invariants_failed = report
        .invariants
        .as_ref()
        .is_some_and(|invariants| !invariants.ok);
    crate::site::HealthCondition {
        code: if invariants_failed {
            "backup.invariants_failed".to_owned()
        } else {
            "backup.verify_failed".to_owned()
        },
        severity: crate::site::HealthSeverity::Critical,
        message: format!(
            "backup {backup_id} failed {} verification: {}",
            report.mode,
            report
                .failure
                .clone()
                .unwrap_or_else(|| "no reason recorded".to_owned())
        ),
    }
}

/// The alert a failed report sends.
pub fn alert_for(report: &VerifyReport, backup_id: &str) -> crate::site::BackupAlert {
    let invariants_failed = report
        .invariants
        .as_ref()
        .is_some_and(|invariants| !invariants.ok);
    crate::site::BackupAlert {
        backup_id: backup_id.to_owned(),
        code: if invariants_failed {
            "backup.invariants_failed".to_owned()
        } else {
            "backup.verify_failed".to_owned()
        },
        message: format!(
            "backup {backup_id} is not restorable: {}",
            report
                .failure
                .clone()
                .unwrap_or_else(|| "verification failed".to_owned())
        ),
    }
}

/// Rejects a `Full` request when no scratch site was wired.
///
/// # Errors
/// [`BackupError::Unsupported`] naming the missing port.
pub fn require_scratch<'a>(
    scratch: Option<&'a dyn ScratchSite>,
    invariants: Option<&'a dyn InvariantRunner>,
) -> Result<(&'a dyn ScratchSite, &'a dyn InvariantRunner)> {
    match (scratch, invariants) {
        (Some(scratch), Some(invariants)) => Ok((scratch, invariants)),
        _ => Err(BackupError::Unsupported {
            feature: "full verification needs a scratch site and an invariant runner".to_owned(),
        }),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::manifest::{BackupManifest, MANIFEST_FORMAT};
    use crate::model::{TargetRef, VerifyMode};
    use crate::package::{PackageEntry, build_from_files};

    fn manifest() -> BackupManifest {
        BackupManifest {
            format: MANIFEST_FORMAT.to_owned(),
            id: "acme-chain-1".to_owned(),
            tenant: "acme".to_owned(),
            created_at: "2026-10-19T12:00:00Z".to_owned(),
            platform_version: "0.1.0".to_owned(),
            snapshot_provider_version: "0.1.0".to_owned(),
            pg_major: 18,
            schema_version: 1,
            app_set: vec!["crm".to_owned()],
            digest: "0".repeat(64),
            size: 0,
            file_count: 0,
            chain_id: "chain".to_owned(),
            chain_sequence: 1,
            parent_digest: None,
            verify: VerifyMode::Quick,
            target: TargetRef::Folder {
                path: "/backups".into(),
            }
            .to_string(),
            encryption: None,
        }
    }

    #[test]
    fn quick_reports_a_changed_byte() {
        let work = tempfile::tempdir().expect("work");
        let built = build_from_files(
            manifest(),
            vec![PackageEntry {
                path: "a".to_owned(),
                bytes: b"abc".to_vec(),
            }],
            work.path(),
        )
        .expect("build");
        assert!(quick(&built.package).ok);
        let mut bytes = std::fs::read(built.package.path()).expect("read");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(built.package.path(), bytes).expect("write");
        let report = quick(&built.package);
        assert!(!report.ok);
        assert!(report.failure.as_ref().expect("reason").contains("digest"));
        assert_eq!(
            condition_for(&report, "acme-chain-1").code,
            "backup.verify_failed"
        );
    }
}
