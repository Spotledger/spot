//! `spot backup list`.
use std::sync::Arc;

use clap::ArgMatches;
use spot_backup::{
    BackupLedger, BackupTarget, FileLedger, FolderTarget, StoredBackup, SystemSpaceProbe,
};

use super::{CommandCtx, Exit, Prepared, Step, prepare, stop_err};

/// The site's stored backups, newest first, and the ledger beside them.
pub(super) fn stored_of(
    ctx: &mut CommandCtx,
    prep: &Prepared,
) -> Step<(Vec<StoredBackup>, FileLedger)> {
    let target = FolderTarget::new(&prep.dir, Arc::new(SystemSpaceProbe));
    let listed = target
        .list()
        .and_then(|all| FileLedger::open(&prep.dir).map(|ledger| (all, ledger)));
    match listed {
        Ok((all, ledger)) => {
            let mine = all
                .into_iter()
                .filter(|b| b.manifest.tenant == prep.site.site)
                .collect();
            Ok((mine, ledger))
        }
        Err(error) => Err(stop_err(ctx, Exit::Failed, &error, &prep.secrets)),
    }
}

pub(super) async fn list_backups(ctx: &mut CommandCtx, sub: &ArgMatches) -> Step<Exit> {
    let prep = prepare(ctx, sub, false).await?;
    let (stored, ledger) = stored_of(ctx, &prep)?;
    let records = ledger.records();
    let _ = writeln!(
        ctx.out(),
        "target {} ({} backups of site {})",
        prep.name,
        stored.len(),
        prep.site.site
    );
    for backup in &stored {
        let verified = records.iter().any(|r| r.id == backup.id && r.verified);
        let sealed = if backup.manifest.encryption.is_some() {
            "encrypted"
        } else {
            "unencrypted"
        };
        let digest: String = backup.manifest.digest.chars().take(12).collect();
        let _ = writeln!(
            ctx.out(),
            "{} {} bytes={} {} {sealed} digest={digest}",
            backup.id,
            backup.manifest.created_at,
            backup.package_bytes,
            if verified { "verified" } else { "unverified" }
        );
    }
    Ok(Exit::Ok)
}
