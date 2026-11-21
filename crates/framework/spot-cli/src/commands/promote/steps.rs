//! The five steps of `spot promote` as the operator runs them.
//!
//! `request`, `approve`, `status` and `abort` change only the promotion's record (and `abort`
//! drops leftover clones); `apply` replays the record through the promotion service with the real
//! ports (see `drive.rs`). Output names counts, digests and ids, never a value. Exit codes: 0
//! done, 1 usage, 2 refused (nothing changed), 3 failed.

use std::sync::Arc;

use clap::ArgMatches;
use spot_bundle_merge::Conflict;
use spot_migration_plan::Risk;
use spot_permissions::Principal;
use spot_promotion::{IssueSource, PlanIssue, PromotionPolicy};

use super::super::bundle::{ImportContext, ImportOutcome, evaluate, load_trust, site_dir};
use super::super::migrate_plan::{Opened, build_lifecycle, on_plain_thread};
use super::super::{CommandCtx, Exit};
use super::audit::{DualAudit, chain_of};
use super::clone::{CloneRunner, drop_leftovers, pg_bin};
use super::drive::{Job, Outcome, run as drive};
use super::env::{Linked, guard_env, link, set_of};
use super::log::{self, ApprovalIntent, Record};
use super::target::{Parts, Publish, load_target};
use super::{APPROVALS_MISSING, check_approvals, check_environments};

fn arg(sub: &ArgMatches, name: &str) -> String {
    sub.get_one::<String>(name).cloned().unwrap_or_default()
}

fn micros(ctx: &CommandCtx) -> i64 {
    i64::try_from(ctx.clock().now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
}

fn created_at(ctx: &CommandCtx) -> spot_cli::Result<String> {
    ctx.clock()
        .now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| spot_cli::Error("the clock cannot be formatted".to_owned()))
}

fn refuse(ctx: &mut CommandCtx, message: &str) -> spot_cli::Result<Exit> {
    writeln!(ctx.err(), "{message}")?;
    Ok(Exit::Refused)
}

/// Both sites of a promotion, linked, and the environment guard passed.
fn ends(
    ctx: &mut CommandCtx,
    from: &str,
    to: &str,
) -> spot_cli::Result<Result<(Linked, Linked), Exit>> {
    if ctx.site(from).is_err() || ctx.site(to).is_err() {
        writeln!(ctx.err(), "there is no such site")?;
        return Ok(Err(Exit::Usage));
    }
    let set = set_of(ctx.root());
    let (source, target) = match (link(ctx.root(), &set, from), link(ctx.root(), &set, to)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(message), _) | (_, Err(message)) => {
            writeln!(ctx.err(), "{message}")?;
            return Ok(Err(Exit::Refused));
        }
    };
    if let Err(message) = check_environments(&guard_env(&source.env), &guard_env(&target.env)) {
        writeln!(ctx.err(), "{message}")?;
        return Ok(Err(Exit::Refused));
    }
    Ok(Ok((source, target)))
}

fn issues_of(outcome: &ImportOutcome) -> Vec<PlanIssue> {
    // Both kinds block: an unresolved conflict means the merged content is not the bundle anyone
    // reviewed, and a conflict with the installed platform is conservatively a stop.
    let convert = |source: IssueSource, conflict: &Conflict| PlanIssue {
        source,
        code: conflict.kind.code().to_owned(),
        item: format!("{}/{}", conflict.section(), conflict.key()),
        message: conflict.explanation.clone(),
        risk: Risk::Destructive,
        blocking: true,
    };
    outcome
        .conflicts
        .iter()
        .map(|c| convert(IssueSource::Merge, c))
        .chain(
            outcome
                .upgrade_conflicts
                .iter()
                .map(|c| convert(IssueSource::Upgrade, c)),
        )
        .collect()
}

/// What the target site needs to be read, planned against and rehearsed.
struct TargetParts {
    parts: Parts,
    catalog: Arc<spot_storage_contract::Catalog>,
    backend: Arc<spot_postgres::PgBackend>,
}

async fn target_parts(ctx: &mut CommandCtx, site: &str) -> spot_cli::Result<TargetParts> {
    let parts = Parts::of_site(ctx.root(), site)?;
    let loaded = spot_cli::config::load(ctx.root(), false)?;
    let site_config = ctx.site(site)?.clone();
    let meta = spot_cli::apps::serving(ctx.root(), &loaded, &site_config)?;
    let catalog = Arc::new(spot_cli::schema::catalog(&meta)?);
    let backend = ctx.backend(site).await?;
    Ok(TargetParts {
        parts,
        catalog,
        backend,
    })
}

/// Evaluates the hand-over file against the target: signature, direction, plan, conflicts.
async fn evaluated(
    ctx: &mut CommandCtx,
    to: &Linked,
    bytes: &[u8],
    actor: &str,
    parts: &TargetParts,
) -> spot_cli::Result<Result<ImportOutcome, String>> {
    let trust = match load_trust(&site_dir(ctx.root(), &to.site), &to.record) {
        Ok(trust) => trust,
        Err(refusal) => return Ok(Err(refusal.to_string())),
    };
    let target = load_target(
        &parts.parts,
        &parts.backend,
        &parts.catalog,
        to.env.clone(),
        actor,
        Publish::Deferred,
    )
    .await
    .map_err(spot_cli::Error)?;
    let now = ctx.clock().now_utc().unix_timestamp();
    let created = created_at(ctx)?;
    Ok(evaluate(
        bytes,
        &ImportContext {
            site: &to.site,
            kind: to.role,
            trust: &trust,
            image: target.image(),
            now,
            created_at: &created,
        },
    )
    .map_err(|refusal| refusal.to_string()))
}

/// `promote request --from A --to B --bundle FILE --as USER`.
pub async fn request(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let (from, to) = (arg(sub, "from"), arg(sub, "to"));
    let requester = arg(sub, "as");
    if from == to {
        return refuse(
            ctx,
            "promote.environment_mismatch: a site cannot be promoted to itself",
        );
    }
    let (_, target) = match ends(ctx, &from, &to)? {
        Ok(pair) => pair,
        Err(exit) => return Ok(exit),
    };
    let Ok(bytes) = std::fs::read(arg(sub, "bundle")) else {
        writeln!(ctx.err(), "the hand-over file cannot be read")?;
        return Ok(Exit::Usage);
    };
    let parts = target_parts(ctx, &to).await?;
    let outcome = match evaluated(ctx, &target, &bytes, &requester, &parts).await? {
        Ok(outcome) => outcome,
        Err(message) => return refuse(ctx, &message),
    };
    let issues = issues_of(&outcome);
    if let Some(first) = issues.first() {
        return refuse(
            ctx,
            &format!(
                "promote.blocked: {} conflict(s) stop this promotion, first {} ({}); resolve them and ask again",
                issues.len(),
                first.item,
                first.code
            ),
        );
    }
    let digest = outcome.bundle.digest().to_hex();
    let at = micros(ctx);
    let id = format!(
        "promo-{}-{}",
        digest.get(..12).unwrap_or(&digest),
        ctx.clock().now_utc().unix_timestamp()
    );
    let record = Record {
        id: id.clone(),
        from_site: from,
        to_site: to,
        requester,
        requested_at: at,
        bundle_digest: digest,
        approvals: Vec::new(),
        state: "requested".to_owned(),
        note: String::new(),
    };
    log::write(ctx.root(), &record, true)?;
    super::super::bundle::write_private(&log::handover_path(ctx.root(), &id), &bytes, false)?;
    let needed = PromotionPolicy::default().for_role(target.role).approvers;
    writeln!(
        ctx.out(),
        "requested promotion {id}: {} change(s) for {}; {needed} approval(s) needed\n  next: spot promote approve {id} --as <APPROVER>",
        outcome.plan.changes().len(),
        target.role.name()
    )?;
    Ok(Exit::Ok)
}

fn load_open(ctx: &mut CommandCtx, id: &str) -> spot_cli::Result<Result<Record, Exit>> {
    match log::read(ctx.root(), id)? {
        None => {
            writeln!(ctx.err(), "there is no such promotion")?;
            Ok(Err(Exit::Usage))
        }
        Some(record) => Ok(Ok(record)),
    }
}

/// `promote approve ID --as USER [--role ROLE]... [--comment TEXT]`.
pub fn approve(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let id = arg(sub, "id");
    let mut record = match load_open(ctx, &id)? {
        Ok(record) => record,
        Err(exit) => return Ok(exit),
    };
    if record.state != "requested" {
        return refuse(
            ctx,
            &format!("promote.bad_state: the promotion is {}", record.state),
        );
    }
    let approver = arg(sub, "as");
    let mut roles: Vec<String> = sub
        .get_many::<String>("role")
        .map(|r| r.cloned().collect())
        .unwrap_or_default();
    if roles.is_empty() {
        roles.push("owner".to_owned());
    }
    let (_, target) = match ends(ctx, &record.from_site.clone(), &record.to_site.clone())? {
        Ok(pair) => pair,
        Err(exit) => return Ok(exit),
    };
    let policy = PromotionPolicy::default();
    let rules = policy.for_role(target.role);
    if rules.forbid_requester && approver == record.requester {
        return refuse(
            ctx,
            &format!(
                "{APPROVALS_MISSING}: the requester cannot approve a promotion to {}",
                target.role.name()
            ),
        );
    }
    if record.approvals.iter().any(|a| a.approver == approver) {
        return refuse(
            ctx,
            &format!("{APPROVALS_MISSING}: {approver} has already approved this promotion"),
        );
    }
    record.approvals.push(ApprovalIntent {
        role: roles[0].clone(),
        roles,
        approver,
        comment: arg(sub, "comment"),
        at: micros(ctx),
    });
    log::write(ctx.root(), &record, false)?;
    writeln!(
        ctx.out(),
        "approval recorded for {id}: {} of {} needed",
        record.approvals.len(),
        rules.approvers
    )?;
    Ok(Exit::Ok)
}

/// `promote status ID`.
pub fn status(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let id = arg(sub, "id");
    let record = match load_open(ctx, &id)? {
        Ok(record) => record,
        Err(exit) => return Ok(exit),
    };
    let names: Vec<&str> = record
        .approvals
        .iter()
        .map(|a| a.approver.as_str())
        .collect();
    writeln!(
        ctx.out(),
        "promotion {}\n  {} -> {}\n  state: {}\n  requested by: {}\n  bundle: {}\n  approvals: {} ({})\n  {}",
        record.id,
        record.from_site,
        record.to_site,
        record.state,
        record.requester,
        record.bundle_digest,
        record.approvals.len(),
        names.join(", "),
        record.note
    )?;
    Ok(Exit::Ok)
}

/// `promote apply ID [--max-rehearsal-age-secs N] [--as USER]`.
pub async fn apply(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let id = arg(sub, "id");
    let mut record = match load_open(ctx, &id)? {
        Ok(record) => record,
        Err(exit) => return Ok(exit),
    };
    if record.state != "requested" {
        return refuse(
            ctx,
            &format!("promote.bad_state: the promotion is {}", record.state),
        );
    }
    let (from, to) = (record.from_site.clone(), record.to_site.clone());
    let (source, target) = match ends(ctx, &from, &to)? {
        Ok(pair) => pair,
        Err(exit) => return Ok(exit),
    };
    let names: Vec<&str> = record
        .approvals
        .iter()
        .map(|a| a.approver.as_str())
        .collect();
    if PromotionPolicy::default().for_role(target.role).approvers > 0
        && let Err(message) = check_approvals(target.role.name(), &record.requester, &names) {
            return refuse(ctx, &message);
        }
    let Some(bin) = pg_bin() else {
        return refuse(
            ctx,
            "pg_dump was not found: set SPOT_PG_BIN to its directory (absolute)",
        );
    };
    let Ok(bytes) = std::fs::read(log::handover_path(ctx.root(), &id)) else {
        return refuse(ctx, "the kept hand-over file of this promotion is missing");
    };
    let actor = format!("user:{}", record.requester);
    let parts = target_parts(ctx, &to).await?;
    let outcome = match evaluated(ctx, &target, &bytes, &actor, &parts).await? {
        Ok(outcome) => outcome,
        Err(message) => return refuse(ctx, &message),
    };
    if outcome.bundle.digest().to_hex() != record.bundle_digest {
        return refuse(
            ctx,
            "promote.bundle_mismatch: the kept file is not the bundle that was requested",
        );
    }
    let source_backend = ctx.backend(&from).await?;
    let (audit_source, audit_target) = match (
        chain_of(&source_backend, &from).await,
        chain_of(&parts.backend, &to).await,
    ) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(message), _) | (_, Err(message)) => return refuse(ctx, &message),
    };
    let handle = tokio::runtime::Handle::current();
    let live = load_target(
        &parts.parts,
        &parts.backend,
        &parts.catalog,
        target.env.clone(),
        &actor,
        Publish::Now(handle.clone()),
    )
    .await
    .map_err(spot_cli::Error)?;
    let clock = Arc::clone(ctx.clock());
    let Opened { store, .. } =
        build_lifecycle(&parts.backend, &to, Arc::clone(&clock), Principal::system())?;
    let runner = CloneRunner {
        handle: handle.clone(),
        site: to.clone(),
        backend: (*parts.backend).clone(),
        catalog: Arc::clone(&parts.catalog),
        parts: parts.parts.clone(),
        bundle: outcome.bundle.clone(),
        env: target.env.clone(),
        clock: Arc::clone(&clock),
        bin,
        promotion: id.clone(),
        actor,
    };
    let job = Job {
        issues: issues_of(&outcome),
        bundle: outcome.bundle,
        record: record.clone(),
        from_env: source.env,
        to_env: target.env,
        target: live,
        runner,
        store,
        audit: Arc::new(DualAudit::new(handle, audit_target, audit_source)),
        clock,
        max_age_secs: sub
            .get_one::<i64>("max-rehearsal-age-secs")
            .copied()
            .unwrap_or(3_600),
    };
    match on_plain_thread(move || drive(job))? {
        Outcome::Done(lines) => {
            record.state = "applied".to_owned();
            record.note = "applied and verified".to_owned();
            log::write(ctx.root(), &record, false)?;
            for line in lines {
                writeln!(ctx.out(), "{line}")?;
            }
            writeln!(ctx.out(), "promotion {id} is verified on {to}")?;
            Ok(Exit::Ok)
        }
        Outcome::Refused(message) => refuse(ctx, &message),
        Outcome::Failed(message) => {
            record.state = "failed".to_owned();
            record.note = message.chars().take(300).collect();
            log::write(ctx.root(), &record, false)?;
            writeln!(ctx.err(), "{message}")?;
            Ok(Exit::Failed)
        }
    }
}

/// `promote abort ID`: marks the promotion aborted and drops the clones it left on the target.
pub async fn abort(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let id = arg(sub, "id");
    let mut record = match load_open(ctx, &id)? {
        Ok(record) => record,
        Err(exit) => return Ok(exit),
    };
    if record.state == "applied" {
        return refuse(
            ctx,
            "promote.bad_state: an applied promotion cannot be aborted; promote a corrected bundle instead",
        );
    }
    let to = record.to_site.clone();
    if ctx.site(&to).is_err() {
        writeln!(ctx.err(), "there is no such site")?;
        return Ok(Exit::Usage);
    }
    let backend = ctx.backend(&to).await?;
    let dropped = match drop_leftovers(&backend, &to, &id).await {
        Ok(dropped) => dropped,
        Err(message) => {
            writeln!(ctx.err(), "{message}")?;
            return Ok(Exit::Failed);
        }
    };
    record.state = "aborted".to_owned();
    record.note = format!("aborted; {} clone(s) dropped", dropped.len());
    log::write(ctx.root(), &record, false)?;
    for name in &dropped {
        writeln!(ctx.out(), "dropped scratch database {name}")?;
    }
    writeln!(
        ctx.out(),
        "promotion {id} aborted; {} clone(s) dropped",
        dropped.len()
    )?;
    Ok(Exit::Ok)
}
