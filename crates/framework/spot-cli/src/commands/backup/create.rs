//! `spot backup run`: take, seal, write and verify one backup.
use std::sync::Arc;

use clap::ArgMatches;
use spot_backup::{
    BackupJob, BackupProducer, BackupRunOutcome, BackupRunner, FileLedger, FolderTarget, KeyRef,
    SnapshotProducer, SystemSpaceProbe, TargetRef, VerifyMode, encrypting,
};
use spot_snapshot::{PgDumpProvider, SnapshotProvider, SnapshotTarget, StoppedInstanceProvider};

use super::scratch::{PgScratch, ScratchInvariantRunner, leftover};
use super::{
    CommandCtx, Exit, Notices, Step, Stop, TARGET_UNAVAILABLE, VersionSource, WorkDir, audit_open,
    connection, exit_of, flush_notices, key_provider, make_private, manual_schedule, millis,
    pg_bin, prepare_with, read_site_version, retention_policy, scrub, stop, stop_err,
    stopped_layout, verdict, verdict_code,
};

/// The one backup chain of a site (ids read `<site>-main-<n>`).
const CHAIN: &str = "main";

pub(super) async fn run_backup(ctx: &mut CommandCtx, sub: &ArgMatches) -> Step<Exit> {
    let stopped = match sub.get_one::<String>("kind").map(String::as_str) {
        None | Some("logical") => false,
        Some("stopped") => true,
        Some(_) => return Err(stop(ctx, Exit::Usage, "--kind must be logical or stopped")),
    };
    // The bundled layout comes from the host, never from the command line; a target inside it is
    // refused (`backup.target_unsafe`) by `prepare_with`.
    let layout = if stopped {
        Some(stopped_layout(ctx)?)
    } else {
        None
    };
    let forbidden = layout.as_ref().map_or_else(Vec::new, |l| {
        vec![l.data_dir.clone(), l.bin_dir.clone(), l.log_dir.clone()]
    });
    let prep = prepare_with(ctx, sub, !stopped, forbidden).await?;
    let bin = match &layout {
        Some(layout) => layout.bin_dir.clone(),
        None => pg_bin(ctx, &prep.loaded.common.backup)?,
    };
    let provider = key_provider(ctx, &prep).await?;
    let key = provider.as_ref().map(|p| {
        let current = p.current();
        KeyRef {
            id: current.id,
            version: current.version,
        }
    });
    if key.is_none() && !sub.get_flag("allow_unencrypted") {
        let text = "backup.encryption_unavailable: the site has no usable key provider ([services.secrets]); nothing was written. Configure one, or pass --allow-unencrypted to store this backup in clear";
        return Err(stop(ctx, Exit::Refused, text));
    }
    // The audit chain lives in the site's database. A logical run records its start first and
    // refuses without it. A stopped-instance run takes place while that database is down, so its
    // record is written once the instance is back (below); the run is then not complete until it
    // is recorded.
    let mut audit = None;
    if !stopped {
        let Some(opened) = audit_open(ctx.clock().clone(), &prep).await else {
            return Err(stop(
                ctx,
                Exit::Refused,
                "backup.audit_unavailable: the audit chain is not available; nothing was changed",
            ));
        };
        opened
            .required("backup_run_started", 0, None)
            .map_err(|e| stop(ctx, Exit::Refused, e.to_string()))?;
        if key.is_none() {
            let reason = Some("backup.encryption_unavailable");
            opened
                .required("backup_run_unencrypted_override", 0, reason)
                .map_err(|e| stop(ctx, Exit::Refused, e.to_string()))?;
        }
        audit = Some(opened);
    }
    // The manifest's schema version is the site's real recorded one. A stopped-instance backup
    // cannot ask the stopped server: it records 0 and says so. A failed read stops the run, it
    // never falls back to a guess.
    let (site_version, version_source) = if stopped {
        (0, VersionSource::Unrecorded)
    } else {
        match read_site_version(&prep).await {
            Ok(found) => found,
            Err(why) => {
                let text = scrub(
                    &format!(
                        "backup.snapshot: the site's schema version could not be read: {why}; nothing was written"
                    ),
                    &prep.secrets,
                );
                if let Some(audit) = &audit {
                    audit.always("backup_run_failed", 0, Some("backup.snapshot"));
                }
                return Err(stop(ctx, Exit::Failed, text));
            }
        }
    };
    if let Some(audit) = &audit {
        audit.always(
            "backup_schema_version",
            site_version,
            Some(version_source.label()),
        );
    }
    let work = match WorkDir::create(&prep.dir) {
        Ok(work) => work,
        Err(error) => {
            let text = format!(
                "{TARGET_UNAVAILABLE}: the target `{}` is not writable by this user ({error}: {}); nothing was written",
                prep.name,
                prep.dir.display()
            );
            return Err(stop(ctx, Exit::Refused, text));
        }
    };
    let _lock = match FileLedger::lock(&prep.dir) {
        Ok(lock) => lock,
        Err(error) => return Err(stop_err(ctx, exit_of(&error), &error, &prep.secrets)),
    };
    let ledger = match FileLedger::open(&prep.dir) {
        Ok(ledger) => ledger,
        Err(error) => return Err(stop_err(ctx, Exit::Failed, &error, &prep.secrets)),
    };
    let pg: Arc<dyn SnapshotProvider>;
    let snapshot_target = match &layout {
        // The password goes to the provider's own safe path and is in `prep.secrets` for scrubbing.
        Some(layout) => {
            pg = Arc::new(StoppedInstanceProvider::new(ctx.clock().clone()));
            SnapshotTarget::Instance {
                layout: layout.clone(),
                tenant: prep.site.site.clone(),
                password: prep.cfg.password.clone(),
            }
        }
        None => {
            let Some(conn) = connection(&prep.cfg, &prep.site.database) else {
                return Err(stop(
                    ctx,
                    Exit::Failed,
                    "the database connection cannot be described",
                ));
            };
            pg = Arc::new(PgDumpProvider::new(bin.clone(), ctx.clock().clone()));
            SnapshotTarget::Database {
                connection: conn,
                tenant: prep.site.site.clone(),
                database: prep.site.database.clone(),
            }
        }
    };
    let inner = SnapshotProducer::new(
        pg,
        snapshot_target,
        work.0.clone(),
        site_version,
        prep.site.apps.clone(),
    );
    let producer: Arc<dyn BackupProducer> = match &provider {
        Some(provider) => Arc::new(encrypting(Arc::new(inner), provider.clone())),
        None => Arc::new(inner),
    };
    let schedule = manual_schedule().map_err(|text| stop(ctx, Exit::Failed, text))?;
    // A stopped-instance backup is verified quick unless --full asks for more.
    let verify = if sub.get_flag("full") {
        VerifyMode::Full
    } else if stopped {
        VerifyMode::Quick
    } else {
        prep.verify
    };
    let job = BackupJob {
        tenant: prep.site.site.clone(),
        schedule,
        target: TargetRef::Folder {
            path: prep.dir.clone(),
        },
        retention: retention_policy(&prep.retention),
        encrypt: key.clone(),
        verify,
        chain_id: CHAIN.to_owned(),
        sequence: 1,
        parent_digest: None,
    };
    let target = FolderTarget::new(&prep.dir, Arc::new(SystemSpaceProbe));
    let notes = Notices::default();
    let scratch = PgScratch::new(prep.cfg.clone(), bin, ctx.clock().clone(), work.0.clone());
    let runner = BackupRunner {
        target: &target,
        producer: producer.as_ref(),
        ledger: &ledger,
        health: &notes,
        alerts: &notes,
        notifier: &notes,
        scratch: Some(&scratch),
        invariants: Some(&ScratchInvariantRunner),
    };
    if stopped {
        let _ = writeln!(
            ctx.out(),
            "stopped-kind backup: service unavailable for the duration"
        );
    }
    let started = ctx.clock().monotonic();
    let result = runner.run(&job, ctx.clock().now_utc()).await;
    let elapsed = millis(ctx.clock().monotonic().saturating_sub(started));
    let left = scratch.cleanup().await;
    flush_notices(ctx, &notes, &prep.secrets);
    if stopped {
        // The provider has started the instance again: record the run now.
        audit = audit_open(ctx.clock().clone(), &prep).await;
        match &audit {
            Some(audit) => {
                if key.is_none() {
                    audit.always(
                        "backup_run_unencrypted_override",
                        0,
                        Some("backup.encryption_unavailable"),
                    );
                }
            }
            None => {
                let _ = writeln!(
                    ctx.err(),
                    "backup.audit_unavailable: the audit chain is not available after the run, so it is not recorded in it"
                );
            }
        }
    }
    let mut exit = Exit::Ok;
    match result {
        Ok(report) => match report.outcome {
            BackupRunOutcome::Created {
                backup,
                verification,
            } => {
                if let Ok(path) = spot_backup::target::package_path(&prep.dir, &backup.id)
                    && make_private(&path).is_err()
                {
                    let _ = writeln!(
                        ctx.err(),
                        "backup.io: the mode of {} could not be restricted to its owner",
                        backup.id
                    );
                    exit = Exit::Failed;
                }
                let sealed = if backup.manifest.encryption.is_some() {
                    "encrypted"
                } else {
                    "unencrypted"
                };
                let checks = verification.checks.join(",");
                writeln!(
                    ctx.out(),
                    "backup ok id={} bytes={} digest={} duration_ms={elapsed} verify={} {sealed} target={} checks={checks} schema_version={} schema_version_source={}",
                    backup.id, backup.package_bytes, backup.manifest.digest, verification.mode, prep.name,
                    backup.manifest.schema_version, version_source.label()
                )
                .map_err(|_| Stop(Exit::Failed))?;
                if key.is_none() {
                    let _ = writeln!(
                        ctx.err(),
                        "unencrypted: --allow-unencrypted was given and the site has no key provider; this backup is stored in clear"
                    );
                }
                let _ = writeln!(
                    ctx.out(),
                    "{}",
                    verdict(verification.mode, verification.invariants.as_ref())
                );
                if let Some(hint) = &prep.eject_hint {
                    let _ = writeln!(ctx.out(), "{hint}");
                }
                if stopped {
                    let _ = writeln!(
                        ctx.out(),
                        "stopped-kind: the copy of the data directory was checked file by file against its checksums; it was not started"
                    );
                    if audit.is_none() {
                        exit = Exit::Failed;
                    }
                }
                for id in &report.deleted {
                    let _ = writeln!(ctx.out(), "pruned {id}");
                }
                if let Some(audit) = &audit {
                    audit.always(
                        "backup_run_succeeded",
                        backup.package_bytes,
                        Some(verdict_code(
                            verification.mode,
                            verification.invariants.as_ref(),
                        )),
                    );
                }
            }
            BackupRunOutcome::WaitingForMedia { .. } => {
                let _ = writeln!(
                    ctx.err(),
                    "{TARGET_UNAVAILABLE}: the target is waiting for media"
                );
                if let Some(audit) = &audit {
                    audit.always("backup_run_failed", 0, Some(TARGET_UNAVAILABLE));
                }
                exit = Exit::Failed;
            }
        },
        Err(error) => {
            let line = scrub(
                &format!("{}: {}", error.code(), error.reason()),
                &prep.secrets,
            );
            let _ = writeln!(ctx.err(), "{line}");
            if let Some(audit) = &audit {
                audit.always("backup_run_failed", 0, Some(error.code()));
            }
            exit = Exit::Failed;
        }
    }
    if leftover(ctx, &left) {
        exit = Exit::Failed;
    }
    Ok(exit)
}
