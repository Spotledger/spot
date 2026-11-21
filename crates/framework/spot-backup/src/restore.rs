//! One-click restore: preflight, service gate, apply, migrate forward, schema checks,
//! L7 invariants, open.
//!
//! The workflow fails closed. Anything after the gate closes leaves it closed; only a
//! restore whose schema checks and invariants all passed opens it again. A backup from
//! a newer schema is refused unless the caller passes [`RestoreOptions::allow_newer_schema`].

use crate::error::{BackupError, Result, join_blockers};
use crate::manifest::BackupManifest;
use crate::package::PackageFile;
use crate::site::{
    InvariantReport, InvariantRunner, MigrationApplied, RestoreTargetSite, SchemaCheck, ServiceGate,
};

/// The command name the UI/CLI registers (`spot restore`).
pub const RESTORE_COMMAND: &str = "restore";

/// Options a restore call carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestoreOptions {
    /// Permit restoring data whose schema is newer than the running platform.
    pub allow_newer_schema: bool,
    /// Replace a populated target instead of refusing.
    pub replace: bool,
}

/// One reason a restore may not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightBlocker {
    /// Stable code (`backup.newer_schema`, `backup.restore_refused`, ...).
    pub code: String,
    /// Plain-language detail.
    pub message: String,
}

/// The preflight matrix's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    /// Whether every check passed.
    pub ok: bool,
    /// One blocker per failed check; empty when `ok`.
    pub blockers: Vec<PreflightBlocker>,
}

impl PreflightReport {
    /// The joined blockers, for a refusal's reason.
    pub fn reason(&self) -> String {
        join_blockers(
            &self
                .blockers
                .iter()
                .map(|blocker| &blocker.message)
                .collect::<Vec<_>>(),
        )
    }
}

/// What the restore ended at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreStatus {
    /// Data, migrations, schema checks and invariants all passed; the service is open.
    Restored {
        /// The forward migration, when the backup was older than the runtime.
        migration: Option<MigrationApplied>,
        /// The invariant report.
        invariants: InvariantReport,
    },
    /// Nothing changed; the preflight refused the restore.
    Refused,
    /// The restore stopped at `stage`; the service gate stays closed.
    Failed {
        /// `service_close`, `apply`, `migrate`, `schema_check`, `invariants`,
        /// `reset_node_local` or `service_open`.
        stage: String,
        /// Plain-language reason.
        reason: String,
    },
}

/// Everything one restore reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// The outcome.
    pub status: RestoreStatus,
    /// The preflight that ran.
    pub preflight: PreflightReport,
    /// The schema checks that ran, when the restore reached them.
    pub schema_checks: Vec<SchemaCheck>,
    /// Whether the service gate is open at the end.
    pub service_open: bool,
}

impl RestoreReport {
    /// Whether the restore succeeded.
    pub fn restored(&self) -> bool {
        matches!(self.status, RestoreStatus::Restored { .. })
    }
}

/// The ports a restore needs: the service gate and the invariant registry.
pub struct RestorePorts<'a> {
    /// Stops and starts the site's service.
    pub service: &'a dyn ServiceGate,
    /// Runs the L7 invariants over the restored site.
    pub invariants: &'a dyn InvariantRunner,
}

/// The preflight matrix. It reads only facts the caller supplied; it never touches the
/// site.
pub fn preflight(
    manifest: &BackupManifest,
    site: &dyn RestoreTargetSite,
    options: &RestoreOptions,
) -> PreflightReport {
    let mut blockers = Vec::new();
    if manifest.tenant != site.tenant() {
        blockers.push(PreflightBlocker {
            code: "backup.restore_refused".to_owned(),
            message: format!(
                "the backup belongs to tenant {} but the target is {}",
                manifest.tenant,
                site.tenant()
            ),
        });
    }
    if manifest.pg_major != site.pg_major() {
        blockers.push(PreflightBlocker {
            code: "backup.restore_refused".to_owned(),
            message: format!(
                "the backup is PostgreSQL {} data but the target runs {}",
                manifest.pg_major,
                site.pg_major()
            ),
        });
    }
    let runtime_schema = site.runtime_schema_version();
    if manifest.schema_version > runtime_schema && !options.allow_newer_schema {
        blockers.push(PreflightBlocker {
            code: "backup.newer_schema".to_owned(),
            message: format!(
                "backup schema {} is newer than the target schema {runtime_schema}; pass allow_newer_schema to override",
                manifest.schema_version
            ),
        });
    }
    let installed = site.app_set();
    let missing: Vec<&String> = manifest
        .app_set
        .iter()
        .filter(|app| !installed.iter().any(|installed| installed == *app))
        .collect();
    if !missing.is_empty() {
        blockers.push(PreflightBlocker {
            code: "backup.restore_refused".to_owned(),
            message: format!(
                "the target does not have these backup apps installed: {}",
                missing
                    .iter()
                    .map(|app| app.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    if site.free_space() < manifest.size {
        blockers.push(PreflightBlocker {
            code: "backup.restore_refused".to_owned(),
            message: format!(
                "the target has {} free bytes and the backup needs {}",
                site.free_space(),
                manifest.size
            ),
        });
    }
    match site.is_empty() {
        Ok(true) => {}
        Ok(false) if options.replace => {}
        Ok(false) => blockers.push(PreflightBlocker {
            code: "backup.restore_refused".to_owned(),
            message: "the target already holds data; pass replace to confirm".to_owned(),
        }),
        Err(error) => blockers.push(PreflightBlocker {
            code: "backup.restore_refused".to_owned(),
            message: format!(
                "the target's emptiness cannot be determined: {}",
                error.reason()
            ),
        }),
    }
    PreflightReport {
        ok: blockers.is_empty(),
        blockers,
    }
}

/// Restores `manifest`/`package` into `site` through the supplied ports.
///
/// This is the function the lead wires into `spot restore`; [`one_click_restore`] is the
/// same call under the command's name.
pub async fn restore(
    manifest: &BackupManifest,
    package: &PackageFile,
    site: &dyn RestoreTargetSite,
    ports: &RestorePorts<'_>,
    options: &RestoreOptions,
) -> RestoreReport {
    if package.manifest() != manifest {
        let blocker = PreflightBlocker {
            code: "backup.manifest_invalid".to_owned(),
            message: "the package header does not match the manifest the restore was asked for"
                .to_owned(),
        };
        return RestoreReport {
            status: RestoreStatus::Refused,
            preflight: PreflightReport {
                ok: false,
                blockers: vec![blocker],
            },
            schema_checks: Vec::new(),
            service_open: ports.service.is_open(),
        };
    }
    // Integrity first: a package whose bytes do not match its manifest digest is never
    // applied, whatever the site implementation does with it later.
    if let Err(error) = crate::package::verify_quick(package) {
        let blocker = PreflightBlocker {
            code: "backup.verify_failed".to_owned(),
            message: format!("the package failed its integrity check: {}", error.reason()),
        };
        return RestoreReport {
            status: RestoreStatus::Refused,
            preflight: PreflightReport {
                ok: false,
                blockers: vec![blocker],
            },
            schema_checks: Vec::new(),
            service_open: ports.service.is_open(),
        };
    }
    let preflight = preflight(manifest, site, options);
    if !preflight.ok {
        return RestoreReport {
            status: RestoreStatus::Refused,
            preflight,
            schema_checks: Vec::new(),
            service_open: ports.service.is_open(),
        };
    }

    if let Err(error) = ports.service.close() {
        return failed(
            preflight,
            "service_close",
            error.reason(),
            ports.service.is_open(),
        );
    }

    if let Err(error) = site.apply(package, options.replace).await {
        return failed(preflight, "apply", error.reason(), ports.service.is_open());
    }

    let runtime_schema = site.runtime_schema_version();
    let migration = if manifest.schema_version < runtime_schema {
        match site
            .migrate_forward(manifest.schema_version, runtime_schema)
            .await
        {
            Ok(migration) => Some(migration),
            Err(error) => {
                return failed(
                    preflight,
                    "migrate",
                    error.reason(),
                    ports.service.is_open(),
                );
            }
        }
    } else {
        None
    };

    let schema_checks = match site.schema_checks() {
        Ok(checks) => checks,
        Err(error) => {
            return failed(
                preflight,
                "schema_check",
                error.reason(),
                ports.service.is_open(),
            );
        }
    };
    let failed_checks: Vec<String> = schema_checks
        .iter()
        .filter(|check| !check.passed)
        .map(|check| format!("{}: {}", check.name, check.detail))
        .collect();
    if !failed_checks.is_empty() {
        return RestoreReport {
            status: RestoreStatus::Failed {
                stage: "schema_check".to_owned(),
                reason: failed_checks.join("; "),
            },
            preflight,
            schema_checks,
            service_open: ports.service.is_open(),
        };
    }

    let invariants = match ports.invariants.run(site).await {
        Ok(report) => report,
        Err(error) => {
            return failed(
                preflight,
                "invariants",
                error.reason(),
                ports.service.is_open(),
            );
        }
    };
    if !invariants.ok {
        return RestoreReport {
            status: RestoreStatus::Failed {
                stage: "invariants".to_owned(),
                reason: invariants.violations.join("; "),
            },
            preflight,
            schema_checks,
            service_open: ports.service.is_open(),
        };
    }

    if let Err(error) = site.reset_node_local().await {
        return RestoreReport {
            status: RestoreStatus::Failed {
                stage: "reset_node_local".to_owned(),
                reason: error.reason(),
            },
            preflight,
            schema_checks,
            service_open: ports.service.is_open(),
        };
    }

    if let Err(error) = ports.service.open() {
        return failed(preflight, "service_open", error.reason(), false);
    }

    RestoreReport {
        status: RestoreStatus::Restored {
            migration,
            invariants,
        },
        preflight,
        schema_checks,
        service_open: true,
    }
}

/// The one-click entry point the UI/CLI registers as `spot restore`.
///
/// # Errors
/// This function returns no error by design: a failure is a [`RestoreStatus::Failed`]
/// report so the UI can show it and the gate stays closed. Only programmer errors (a
/// package whose manifest cannot be read) are impossible here because both arguments
/// are already opened.
pub async fn one_click_restore(
    manifest: &BackupManifest,
    package: &PackageFile,
    site: &dyn RestoreTargetSite,
    ports: &RestorePorts<'_>,
    options: &RestoreOptions,
) -> RestoreReport {
    restore(manifest, package, site, ports, options).await
}

fn failed(
    preflight: PreflightReport,
    stage: &str,
    reason: String,
    service_open: bool,
) -> RestoreReport {
    RestoreReport {
        status: RestoreStatus::Failed {
            stage: stage.to_owned(),
            reason,
        },
        preflight,
        schema_checks: Vec::new(),
        service_open,
    }
}

/// Builds the refusal error a host may want instead of a report.
///
/// # Errors
/// Always [`BackupError::RestoreRefused`] when the report is not ok.
pub fn refusal_error(report: &PreflightReport) -> Result<()> {
    if report.ok {
        Ok(())
    } else {
        Err(BackupError::RestoreRefused {
            reason: report.reason(),
        })
    }
}
