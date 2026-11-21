//! `spot restore apply`: the gated restore and every way back from a failure.

use clap::ArgMatches;
use spot_backup::{
    BackupError, PreflightBlocker, PreflightReport, RestorePorts, RestoreStatus, RestoredSite,
    quick,
};
use spot_cli::config::Site;
use spot_cli::services::secrets as svc;
use spot_secrets::KeyProvider;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::guard::{LockState, StateLock, check_token, fault, root_check, spend_token, token_path};
use super::server::{busy, quiesce, recover_stale, reopen, swap, unswap};
use super::site::{
    Flags, GateFlag, PgSite, ShippedInvariants, assess, audit, build_site, open_package,
    print_blockers, target_text,
};
use super::{
    CommandCtx, Exit, Prepared, Step, Stop, pg_bin, prepare, private_dir, say, scrub, state_dir,
    stop, valid_id,
};

/// Removes a plaintext package on every way out.
pub(super) struct RemoveOnDrop(Option<PathBuf>);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The scratch work directory under the site's state directory, removed when dropped.
pub(super) struct WorkDir(PathBuf);
impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) async fn apply_verb(ctx: &mut CommandCtx, sub: &ArgMatches) -> Step<Exit> {
    let (Some(id), Some(token)) = (
        sub.get_one::<String>("from").cloned(),
        sub.get_one::<String>("confirm").cloned(),
    ) else {
        return Err(stop(
            ctx,
            Exit::Usage,
            "--from and --confirm are required; run `spot restore preflight` first",
        ));
    };
    if !valid_id(&id) {
        return Err(stop(
            ctx,
            Exit::Usage,
            "--from must be a backup id (letters, digits, '.', '_' and '-')",
        ));
    }
    let new_site = sub.get_one::<String>("as_new_site").cloned();
    if new_site
        .as_deref()
        .is_some_and(|n| !spot_cli::config::site_name(n))
    {
        return Err(stop(ctx, Exit::Usage, "--as-new-site must be a site name"));
    }
    let flags = Flags {
        accept_unverified: sub.get_flag("accept_unverified_schema"),
        migrate_forward: sub.get_flag("migrate_forward"),
    };
    let prep = prepare(ctx, sub, true)?;
    let refuse = |ctx: &mut CommandCtx, code: &str, text: String| {
        let _ = writeln!(ctx.err(), "{text}");
        let _ = code;
        Stop(Exit::Refused)
    };
    if let Err(text) = root_check(ctx.root()) {
        return Err(refuse(ctx, "restore.user", text));
    }
    let package = open_package(ctx, &prep, &id).await?;
    let manifest = package.manifest().clone();
    let target = target_text(new_site.as_deref(), &prep.site.site);
    let token_file = match token_path(ctx.root(), &prep.site.site) {
        Ok(path) => path,
        Err(_) => {
            return Err(stop(
                ctx,
                Exit::Failed,
                "backup.io: the site's state directory cannot be used",
            ));
        }
    };
    let now = ctx.clock().now_utc().unix_timestamp();
    if let Err(text) = check_token(&token_file, &token, &id, &target, now) {
        let _ = audit(
            ctx,
            &prep,
            "restore_refused",
            0,
            Some("restore.confirm_invalid"),
            false,
        )
        .await;
        return Err(refuse(ctx, "restore.confirm_invalid", text));
    }
    let verdict = quick(&package);
    if !verdict.ok {
        let text = format!(
            "backup.verify_failed: the package failed its integrity check: {}; nothing was changed and the site's gate was not touched",
            verdict.failure.unwrap_or_default()
        );
        let _ = audit(
            ctx,
            &prep,
            "restore_refused",
            0,
            Some("backup.verify_failed"),
            false,
        )
        .await;
        return Err(refuse(ctx, "backup.verify_failed", text));
    }
    let bin = pg_bin().map_err(|text| refuse(ctx, "backup.snapshot", text))?;
    let state = state_dir(ctx.root(), &prep.site.site).map_err(|_| {
        stop(
            ctx,
            Exit::Failed,
            "backup.io: the site's state directory cannot be used",
        )
    })?;
    if let Err(text) = recover_stale(ctx, &prep).await {
        return Err(stop(
            ctx,
            Exit::Failed,
            format!("restore.recovery_failed: {text}"),
        ));
    }
    let lock = match StateLock::acquire(&state) {
        Ok(lock) => lock,
        Err(LockState::Failed) => {
            return Err(stop(
                ctx,
                Exit::Failed,
                "backup.io: the restore lock cannot be created",
            ));
        }
        Err(_) => {
            return Err(refuse(
                ctx,
                "restore.busy",
                "restore.busy: another restore of this site is running".to_owned(),
            ));
        }
    };
    let work = WorkDir(state.join(format!("restore-work-{}", std::process::id())));
    private_dir(&work.0).map_err(|_| {
        stop(
            ctx,
            Exit::Failed,
            "backup.io: the work directory cannot be created",
        )
    })?;
    let site = build_site(
        ctx,
        &prep,
        &manifest,
        new_site.as_deref(),
        flags,
        work.0.clone(),
    )
    .await?;
    let site = PgSite { bin, ..site };
    let (report, _) = assess(&manifest, &site, true, flags);
    let mut blockers = report.blockers.clone();
    if let Some(name) = &new_site {
        if ctx.root().join("sites").join(name).exists() || site.exists {
            let message = format!(
                "a site or database called `{name}` already exists; restore never overwrites through --as-new-site"
            );
            blockers.push(PreflightBlocker {
                code: "restore.target_exists".to_owned(),
                message,
            });
        }
    } else if !site.exists {
        blockers.push(PreflightBlocker {
            code: "restore.target_missing".to_owned(),
            message: "the site's database does not exist; use --as-new-site".to_owned(),
        });
    } else if let Ok(Some(text)) | Err(text) = busy(&prep.cfg, &site.live, &prep.site.site).await {
        blockers.push(PreflightBlocker {
            code: "restore.site_busy".to_owned(),
            message: text,
        });
    }
    if !blockers.is_empty() {
        let code = blockers[0].code.clone();
        print_blockers(
            ctx,
            &PreflightReport {
                ok: false,
                blockers,
            },
        );
        let _ = audit(ctx, &prep, "restore_refused", 0, Some(&code), false).await;
        return Ok(Exit::Refused);
    }
    // The key (when sealed) and the plaintext package, before anything is closed.
    let mut plain = RemoveOnDrop(None);
    let mut opened = package.clone();
    if manifest.encryption.is_some() {
        let provider = match svc::open(ctx.root(), &prep.loaded, &prep.site).await {
            Ok(Some(host)) => {
                let provider: Arc<dyn KeyProvider> = host.keyring().clone();
                provider
            }
            Ok(None) => return Err(refuse(ctx, "backup.encryption_unavailable", "backup.encryption_unavailable: the backup is encrypted and the site has no key provider ([services.secrets])".to_owned())),
            Err(error) => return Err(refuse(ctx, "backup.encryption_unavailable", scrub(&error.to_string(), &prep.secrets))),
        };
        match spot_backup::package::decrypt_package(&package, provider.as_ref()).await {
            Ok(decrypted) => {
                plain.0 = Some(decrypted.path().to_path_buf());
                opened = decrypted;
            }
            Err(error) => {
                let text = scrub(
                    &format!("{}: {}; nothing was changed", error.code(), error.reason()),
                    &prep.secrets,
                );
                let _ = audit(ctx, &prep, "restore_refused", 0, Some(error.code()), false).await;
                return Err(refuse(ctx, error.code(), text));
            }
        }
    }
    let _ledger_lock = match spot_backup::FileLedger::lock(&prep.dir) {
        Ok(lock) => Some(lock),
        Err(BackupError::LedgerLocked { .. }) => return Err(refuse(ctx, "backup.ledger_locked", "backup.ledger_locked: a backup run or prune holds the target; try again when it is done".to_owned())),
        Err(_) => None,
    };
    let count = manifest.size;
    let reason = flags
        .accept_unverified
        .then_some("unverified_schema_accepted");
    if let Err(text) = audit(ctx, &prep, "restore_started", count, reason, true).await {
        return Err(refuse(
            ctx,
            "backup.audit_unavailable",
            format!("{text}; nothing was changed"),
        ));
    }
    if let Err(text) = spend_token(&token_file) {
        return Err(refuse(ctx, "restore.confirm_invalid", text));
    }
    let new_dir = new_site.as_ref().map(|n| ctx.root().join("sites").join(n));
    if let Some(dir) = &new_dir
        && private_dir(dir).is_err()
    {
        return Err(refuse(
            ctx,
            "restore.target_exists",
            "restore.target_exists: the new site's directory could not be reserved".to_owned(),
        ));
    }
    fault("before_gate");
    let proven = if site.exists {
        match quiesce(ctx, &prep).await {
            Ok(()) => true,
            Err(text) => {
                return Ok(abort(
                    ctx,
                    &prep,
                    &site,
                    new_dir.as_deref(),
                    &format!("gate: {text}"),
                    Back::Reopen,
                )
                .await);
            }
        }
    } else {
        true
    };
    fault("after_gate");
    let flag = GateFlag {
        proven: AtomicBool::new(proven),
        open: AtomicBool::new(true),
    };
    let ports = RestorePorts {
        service: &flag,
        invariants: &ShippedInvariants,
    };
    let options = spot_backup::RestoreOptions {
        allow_newer_schema: false,
        replace: site.exists,
    };
    let finished = spot_backup::restore(opened.manifest(), &opened, &site, &ports, &options).await;
    let (migration, invariants) = match finished.status {
        RestoreStatus::Restored {
            migration,
            invariants,
        } => (migration, invariants),
        RestoreStatus::Refused => {
            let text = format!("preflight refused: {}", finished.preflight.reason());
            return Ok(abort(
                ctx,
                &prep,
                &site,
                new_dir.as_deref(),
                &text,
                live_back(&site),
            )
            .await);
        }
        RestoreStatus::Failed { stage, reason } => {
            let text = format!("stage={stage} {reason}");
            return Ok(abort(
                ctx,
                &prep,
                &site,
                new_dir.as_deref(),
                &text,
                live_back(&site),
            )
            .await);
        }
    };
    say(
        ctx,
        format!(
            "restored into the scratch database; {} invariant checks passed; node-local state reset",
            invariants.checks_run
        ),
    );
    fault("before_swap");
    let Some(scratch) = site.scratch_name() else {
        return Ok(abort(
            ctx,
            &prep,
            &site,
            new_dir.as_deref(),
            "stage=swap the scratch database is unknown",
            live_back(&site),
        )
        .await);
    };
    let secs = ctx.clock().now_utc().unix_timestamp();
    let kept = match swap(
        &prep.cfg,
        &site.live,
        &scratch,
        &prep.site.site,
        secs,
        site.exists,
    )
    .await
    {
        Ok(kept) => kept,
        Err(text) => {
            return Ok(abort(
                ctx,
                &prep,
                &site,
                new_dir.as_deref(),
                &format!("stage=swap {text}"),
                live_back(&site),
            )
            .await);
        }
    };
    fault("after_swap");
    let opened_again = if site.exists {
        reopen(&prep.cfg, &site.live, &prep.site.site, ctx.clock().as_ref()).await
    } else {
        write_new_site(ctx, &prep, new_site.as_deref(), &site)
    };
    if let Err(text) = opened_again {
        let back = unswap(&prep.cfg, &site.live, kept.as_deref(), secs).await;
        let tail = if back.is_ok() {
            "the previous database is back"
        } else {
            "the previous database could NOT be put back; see the quarantine database"
        };
        let line = format!("stage=open {text}; {tail}");
        return Ok(abort(
            ctx,
            &prep,
            &site,
            new_dir.as_deref(),
            &line,
            if back.is_ok() && site.exists {
                Back::Reopen
            } else {
                Back::Told
            },
        )
        .await);
    }
    let _ = spot_cli::request_reload(ctx.root(), &prep.site.site);
    let version = site.schema_version();
    let schema = if site.verified {
        version.to_string()
    } else {
        format!("{version} (unverified, accepted)")
    };
    let steps = migration
        .or_else(|| site.applied().migrated.clone())
        .map_or_else(
            || "none".to_owned(),
            |m| format!("{} (schema {} to {})", m.steps.len(), m.from, m.to),
        );
    let _ = writeln!(
        ctx.out(),
        "restore ok site={} backup={id} schema={schema} migration_steps={steps} target={}",
        prep.site.site,
        new_site.as_deref().unwrap_or("replaced")
    );
    if let Some(kept) = &kept {
        let _ = writeln!(
            ctx.out(),
            "the replaced database is kept as {kept}; `spot restore purge-quarantine --site {} --older-than 7d --apply` removes it later (never within 24 h)",
            prep.site.site
        );
    }
    if let Some(name) = &new_site {
        let _ = writeln!(
            ctx.out(),
            "the new site `{name}` has no password yet: set one with `spot set-password`"
        );
    }
    let left = site.cleanup().await;
    if let Some(name) = left {
        let _ = writeln!(
            ctx.err(),
            "backup.scratch_left: the scratch database `{name}` could not be dropped; drop it"
        );
    }
    if audit(ctx, &prep, "restore_succeeded", count, reason, false)
        .await
        .is_err()
    {
        let _ = writeln!(
            ctx.err(),
            "backup.audit_unavailable: the restored database has no audit chain to record this restore in"
        );
    }
    drop(lock);
    Ok(Exit::Ok)
}

pub(super) fn live_back(site: &PgSite) -> Back {
    if site.exists {
        Back::Reopen
    } else {
        Back::Nothing
    }
}

/// Creates the new site's `site.toml` after its database exists.
pub(super) fn write_new_site(
    ctx: &CommandCtx,
    prep: &Prepared,
    name: Option<&str>,
    site: &PgSite,
) -> Result<(), String> {
    let Some(name) = name else { return Ok(()) };
    let new = Site::named(
        name,
        &site.live,
        &prep.site.collation,
        prep.site.apps.clone(),
    );
    let path = ctx.root().join("sites").join(name).join("site.toml");
    spot_cli::sitefile::create(&path, &new)
        .map_err(|_| "the new site's site.toml could not be written".to_owned())
}

/// How far a failed restore got, which decides what its failure line may claim.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Back {
    /// Nothing had begun.
    Nothing,
    /// The previous database must be put in service and the gate opened.
    Reopen,
    /// The failure line already says where things stand.
    Told,
}

/// Every way back from a failure: drop the scratch, release the new site's directory, put the
/// previous database in service and open the gate. Prints the single failure line and returns
/// exit 3 (refused, 2, when nothing had begun).
pub(super) async fn abort(
    ctx: &mut CommandCtx,
    prep: &Prepared,
    site: &PgSite,
    new_dir: Option<&Path>,
    why: &str,
    back: Back,
) -> Exit {
    let left = site.cleanup().await;
    if let Some(dir) = new_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
    let done = match back {
        Back::Reopen => reopen(&prep.cfg, &site.live, &prep.site.site, ctx.clock().as_ref())
            .await
            .map(|()| "the previous database is in service and the gate is open".to_owned()),
        Back::Nothing => Ok("nothing was changed".to_owned()),
        Back::Told => Ok("see above".to_owned()),
    };
    let mut tail = match done {
        Ok(text) => text,
        Err(text) => format!("the previous database is intact but the gate stays closed ({text})"),
    };
    if let Some(name) = left {
        tail += &format!("; the scratch database `{name}` must be dropped by hand");
    }
    let line = scrub(&format!("restore.failed: {why}; {tail}"), &prep.secrets);
    let _ = writeln!(ctx.err(), "{line}");
    let _ = audit(
        ctx,
        prep,
        "restore_failed",
        0,
        Some("restore.failed"),
        false,
    )
    .await;
    Exit::Failed
}
