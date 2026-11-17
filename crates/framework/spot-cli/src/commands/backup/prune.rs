//! `spot backup prune`.
use std::sync::Arc;

use clap::ArgMatches;
use spot_backup::{
    BackupLedger, BackupRecord, BackupTarget, FileLedger, FolderTarget, SystemSpaceProbe,
};

use super::list::stored_of;
use super::{
    CommandCtx, Exit, Step, audit_open, exit_of, prepare, retention_policy, stop, stop_err,
};

pub(super) async fn prune_backups(ctx: &mut CommandCtx, sub: &ArgMatches) -> Step<Exit> {
    let apply = sub.get_flag("apply");
    let prep = prepare(ctx, sub, apply).await?;
    let _lock = match FileLedger::lock(&prep.dir) {
        Ok(lock) => lock,
        Err(error) => return Err(stop_err(ctx, exit_of(&error), &error, &prep.secrets)),
    };
    let (stored, ledger) = stored_of(ctx, &prep)?;
    let ledger_records = ledger.records();
    let mut records = Vec::new();
    for backup in &stored {
        let known = ledger_records.iter().find(|r| r.id == backup.id);
        let (verified, open) = known.map_or((false, false), |r| (r.verified, r.open_restore));
        match BackupRecord::from_manifest(&backup.manifest, verified, open) {
            Ok(record) => records.push(record),
            Err(error) => return Err(stop_err(ctx, Exit::Failed, &error, &prep.secrets)),
        }
    }
    records.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.sequence.cmp(&a.sequence))
    });
    let plan = match spot_backup::retention::plan(
        &retention_policy(&prep.retention),
        &records,
        ctx.clock().now_utc(),
    ) {
        Ok(plan) => plan,
        Err(error) => return Err(stop_err(ctx, Exit::Refused, &error, &prep.secrets)),
    };
    // The library plan already keeps the `keep_last` newest and the last verified backup; the
    // command removes exactly what it lists, nothing it computed itself.
    let last_good = records.iter().find(|r| r.verified).map(|r| r.id.clone());
    let doomed: Vec<String> = plan.delete.clone();
    for decision in &plan.keep {
        let _ = writeln!(
            ctx.out(),
            "keep {} ({})",
            decision.id,
            decision.reason.as_str()
        );
    }
    for id in &doomed {
        let _ = writeln!(ctx.out(), "remove {id}");
    }
    if !apply {
        let _ = writeln!(
            ctx.out(),
            "dry run: {} backups would be removed; add --apply to remove them",
            doomed.len()
        );
        return Ok(Exit::Ok);
    }
    if last_good.is_none() && !doomed.is_empty() {
        let text = "backup.retention_refused: no backup of this site is verified, so none is removed; run `spot backup verify` first";
        return Err(stop(ctx, Exit::Refused, text));
    }
    let Some(audit) = audit_open(ctx.clock().clone(), &prep).await else {
        return Err(stop(
            ctx,
            Exit::Refused,
            "backup.audit_unavailable: the audit chain is not available; nothing was removed",
        ));
    };
    let count = u64::try_from(doomed.len()).unwrap_or(u64::MAX);
    audit
        .required("backup_prune_started", count, None)
        .map_err(|e| stop(ctx, Exit::Refused, e.to_string()))?;
    let target = FolderTarget::new(&prep.dir, Arc::new(SystemSpaceProbe));
    for id in &doomed {
        let removed = target.delete(id).and_then(|()| ledger.try_remove(id));
        if let Err(error) = removed {
            audit.always("backup_prune_failed", 0, Some(error.code()));
            return Err(stop_err(ctx, Exit::Failed, &error, &prep.secrets));
        }
        let _ = writeln!(ctx.out(), "removed {id}");
    }
    audit.always("backup_prune_succeeded", count, None);
    Ok(Exit::Ok)
}
