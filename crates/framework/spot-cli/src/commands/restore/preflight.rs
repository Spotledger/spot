//! `spot restore preflight`.

use clap::ArgMatches;
use spot_backup::{PreflightBlocker, quick};
use std::path::PathBuf;

use super::guard::{issue_token, root_check};
use super::server::{busy, recover_stale};
use super::site::{Flags, assess, audit, build_site, open_package, print_blockers, target_text};
use super::{CommandCtx, Exit, Step, TOKEN_TTL_SECS, prepare, stop, valid_id};

pub(super) async fn preflight_verb(ctx: &mut CommandCtx, sub: &ArgMatches) -> Step<Exit> {
    let Some(id) = sub
        .get_one::<String>("from")
        .cloned()
        .filter(|id| valid_id(id))
    else {
        return Err(stop(
            ctx,
            Exit::Usage,
            "--from must be a backup id (letters, digits, '.', '_' and '-')",
        ));
    };
    let new_site = sub.get_one::<String>("as_new_site").cloned();
    if new_site
        .as_deref()
        .is_some_and(|n| !spot_cli::config::site_name(n))
    {
        return Err(stop(ctx, Exit::Usage, "--as-new-site must be a site name"));
    }
    let prep = prepare(ctx, sub, true)?;
    if let Err(text) = recover_stale(ctx, &prep).await {
        return Err(stop(
            ctx,
            Exit::Failed,
            format!("restore.recovery_failed: {text}"),
        ));
    }
    let package = open_package(ctx, &prep, &id).await?;
    let manifest = package.manifest().clone();
    let verdict = quick(&package);
    let site = build_site(
        ctx,
        &prep,
        &manifest,
        new_site.as_deref(),
        Flags {
            accept_unverified: true,
            migrate_forward: true,
        },
        PathBuf::new(),
    )
    .await?;
    let (mut report, mut notes) = assess(&manifest, &site, false, Flags::default());
    if !verdict.ok {
        let why = verdict.failure.unwrap_or_default();
        report.blockers.push(PreflightBlocker {
            code: "backup.verify_failed".to_owned(),
            message: format!("the package failed its integrity check: {why}"),
        });
    }
    if let Some(name) = &new_site {
        if ctx.root().join("sites").join(name).exists() || site.exists {
            let message = format!(
                "a site or database called `{name}` already exists; restore never overwrites a site by creating one"
            );
            report.blockers.push(PreflightBlocker {
                code: "restore.target_exists".to_owned(),
                message,
            });
        }
    } else if site.exists {
        match busy(&prep.cfg, &site.live, &prep.site.site).await {
            Ok(None) => {}
            Ok(Some(text)) => report.blockers.push(PreflightBlocker {
                code: "restore.site_busy".to_owned(),
                message: text,
            }),
            Err(text) => report.blockers.push(PreflightBlocker {
                code: "restore.site_busy".to_owned(),
                message: text,
            }),
        }
    } else {
        report.blockers.push(PreflightBlocker {
            code: "restore.target_missing".to_owned(),
            message: "the site's database does not exist; use --as-new-site".to_owned(),
        });
    }
    if let Err(text) = root_check(ctx.root()) {
        report.blockers.push(PreflightBlocker {
            code: "restore.user".to_owned(),
            message: text,
        });
    }
    report.ok = report.blockers.is_empty();
    notes.push("approved migration plans: the platform keeps no approved-plan store yet, so only a closed gate and a running migration are checked".to_owned());
    let out = ctx.out();
    let _ = writeln!(
        out,
        "preflight site={} backup={id} target={}",
        prep.site.site, prep.name
    );
    let _ = writeln!(
        out,
        "taken_at={} size_bytes={} package={}",
        manifest.created_at,
        manifest.size,
        if verdict.ok {
            "verified (digest, size and header; nothing was restored)"
        } else {
            "FAILED"
        }
    );
    let _ = writeln!(
        out,
        "pg_major={} free_bytes_estimate={}",
        manifest.pg_major,
        if site.free == u64::MAX {
            "unknown".to_owned()
        } else {
            site.free.to_string()
        }
    );
    match (&new_site, site.exists) {
        (Some(name), _) => {
            let _ = writeln!(
                out,
                "target=new site `{name}` (database {}); the site `{}` is not touched",
                site.live, prep.site.site
            );
        }
        (None, true) => {
            let _ = writeln!(
                out,
                "replaces=database {} (kept afterwards as {}_replaced_<time>)",
                site.live,
                site.live.chars().take(40).collect::<String>()
            );
        }
        (None, false) => {
            let _ = writeln!(out, "target=missing");
        }
    }
    for note in &notes {
        let _ = writeln!(out, "{note}");
    }
    if !report.ok {
        print_blockers(ctx, &report);
        let code = report.blockers.first().map(|b| b.code.clone());
        let _ = audit(ctx, &prep, "restore_refused", 0, code.as_deref(), false).await;
        return Ok(Exit::Refused);
    }
    let now = ctx.clock().now_utc().unix_timestamp();
    let target = target_text(new_site.as_deref(), &prep.site.site);
    match issue_token(ctx.root(), &prep.site.site, &id, &target, now) {
        Ok(token) => {
            let _ = writeln!(
                ctx.out(),
                "confirm_token={token} expires_in={TOKEN_TTL_SECS}s"
            );
            let _ = writeln!(
                ctx.out(),
                "next: spot restore apply --site {} --from {id} --confirm <token>",
                prep.site.site
            );
            Ok(Exit::Ok)
        }
        Err(_) => Err(stop(
            ctx,
            Exit::Failed,
            "backup.io: the confirm token could not be stored in the site's state directory",
        )),
    }
}
