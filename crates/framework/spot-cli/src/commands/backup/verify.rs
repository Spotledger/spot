//! `spot backup verify`.
use clap::ArgMatches;
use spot_backup::{
    AlertSink, BackupError, BackupLedger, BackupManifest, BackupRecord, FileLedger, full,
    full_with_provider, quick,
};

use super::list::stored_of;
use super::scratch::{PgScratch, ScratchInvariantRunner, leftover};
use super::{
    CommandCtx, Exit, Notices, Step, TARGET_UNAVAILABLE, WorkDir, audit_open, exit_of,
    flush_notices, key_provider, pg_bin, prepare, stop, stop_err, valid_id, verdict, verdict_code,
};

/// Marks `manifest`'s backup verified or not in the ledger, keeping an open-restore flag.
fn record_verdict(
    ledger: &FileLedger,
    manifest: &BackupManifest,
    verified: bool,
) -> spot_backup::Result<()> {
    let open = ledger
        .records()
        .iter()
        .any(|r| r.id == manifest.id && r.open_restore);
    ledger.try_upsert(BackupRecord::from_manifest(manifest, verified, open)?)
}

pub(super) async fn verify_backup(ctx: &mut CommandCtx, sub: &ArgMatches) -> Step<Exit> {
    let wanted = sub.get_one::<String>("id").cloned();
    if wanted.as_deref().is_some_and(|id| !valid_id(id)) {
        return Err(stop(
            ctx,
            Exit::Usage,
            "--id must be a backup id (letters, digits, '.', '_' and '-')",
        ));
    }
    let prep = prepare(ctx, sub, false).await?;
    let deep = sub.get_flag("full");
    let _lock = match FileLedger::lock(&prep.dir) {
        Ok(lock) => lock,
        Err(error) => return Err(stop_err(ctx, exit_of(&error), &error, &prep.secrets)),
    };
    let id = match wanted {
        Some(id) => id,
        None => match stored_of(ctx, &prep)?.0.first() {
            Some(newest) => newest.id.clone(),
            None => {
                return Err(stop(
                    ctx,
                    Exit::Refused,
                    "backup.not_found: no backup of this site exists on the target",
                ));
            }
        },
    };
    let path = match spot_backup::target::package_path(&prep.dir, &id) {
        Ok(path) => path,
        Err(error) => return Err(stop_err(ctx, Exit::Usage, &error, &prep.secrets)),
    };
    // A symbolic link is never followed: only a regular file in the target is a backup.
    if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        return Err(stop(
            ctx,
            Exit::Refused,
            format!("backup.not_found: no backup {id} exists on the target"),
        ));
    }
    let ledger = match FileLedger::open(&prep.dir) {
        Ok(ledger) => ledger,
        Err(error) => return Err(stop_err(ctx, Exit::Failed, &error, &prep.secrets)),
    };
    let audit = audit_open(ctx.clock().clone(), &prep).await;
    if audit.is_none() {
        let _ = writeln!(
            ctx.err(),
            "backup.audit_unavailable: the audit chain is not available; this verification is not recorded in it"
        );
    }
    let notes = Notices::default();
    let mut left = Vec::new();
    let outcome = match spot_backup::package::open(&path) {
        Err(error) => Err(error),
        Ok(package) if package.manifest().tenant != prep.site.site => {
            return Err(stop(
                ctx,
                Exit::Refused,
                format!("backup.restore_refused: backup {id} belongs to another site"),
            ));
        }
        Ok(package) if !deep => Ok((package.manifest().clone(), quick(&package))),
        Ok(package) => {
            let provider = key_provider(ctx, &prep).await?;
            let bin = pg_bin(ctx, &prep.loaded.common.backup)?;
            let work = match WorkDir::create(&prep.dir) {
                Ok(work) => work,
                Err(_) => {
                    return Err(stop(
                        ctx,
                        Exit::Refused,
                        format!(
                            "{TARGET_UNAVAILABLE}: the target is not writable by this user, so a full verification cannot restore into a scratch database"
                        ),
                    ));
                }
            };
            let scratch =
                PgScratch::new(prep.cfg.clone(), bin, ctx.clock().clone(), work.0.clone());
            let report = match (&provider, package.manifest().encryption.is_some()) {
                (Some(p), true) => {
                    full_with_provider(&package, p.as_ref(), &scratch, &ScratchInvariantRunner)
                        .await
                }
                (None, true) => Err(BackupError::EncryptionUnavailable {
                    reason: "the backup is encrypted and the site has no key provider".to_owned(),
                }),
                (_, false) => full(&package, &scratch, &ScratchInvariantRunner).await,
            };
            left = scratch.cleanup().await;
            report.map(|report| (package.manifest().clone(), report))
        }
    };
    let (exit, text, reason) = match outcome {
        Ok((manifest, report)) if report.ok => {
            if let Err(error) = record_verdict(&ledger, &manifest, true) {
                return Err(stop_err(ctx, Exit::Failed, &error, &prep.secrets));
            }
            (
                Exit::Ok,
                format!(
                    "verify ok id={id} mode={} checks={}\n{}",
                    report.mode,
                    report.checks.join(","),
                    verdict(report.mode, report.invariants.as_ref())
                ),
                Some(verdict_code(report.mode, report.invariants.as_ref()).to_owned()),
            )
        }
        Ok((manifest, report)) => {
            let alert = spot_backup::verify::alert_for(&report, &id);
            let code = alert.code.clone();
            notes.alert(alert);
            if let Err(error) = record_verdict(&ledger, &manifest, false) {
                notes.push(format!("{}: {}", error.code(), error.reason()));
            }
            (Exit::Failed, format!("verify FAILED id={id}"), Some(code))
        }
        Err(error) => {
            let code = error.code();
            notes.push(format!(
                "ALERT {code}: backup {id} could not be verified: {}",
                error.reason()
            ));
            (
                Exit::Failed,
                format!("verify FAILED id={id}"),
                Some(code.to_owned()),
            )
        }
    };
    flush_notices(ctx, &notes, &prep.secrets);
    let _ = writeln!(ctx.out(), "{text}");
    if let Some(audit) = &audit {
        audit.always("backup_verify", 1, reason.as_deref());
    }
    Ok(if leftover(ctx, &left) {
        Exit::Failed
    } else {
        exit
    })
}
