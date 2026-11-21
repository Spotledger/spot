//! `spot promote`: request, approve, status, apply and abort a promotion between two sites
//! (WP-ALM-04.T4). Guide: `docs/running-spot/promote.md`.
//!
//! The state machine, policy and approvals live in `spot-promotion`; this file is the operator's
//! surface and the **guards that run before any port is touched**: environment identity on both
//! ends, the production rule (two approvers, one not the requester) and the rehearsal
//! freshness rule. Exit codes: 0 done, 1 usage, 2 refused (nothing changed), 3 failed.
//!
//! Wiring (WP-ALM-04.T5): `request` evaluates the signed hand-over file against the target
//! (`spot bundle import`'s checks) and records the promotion; `approve` records an approver's
//! intent after the same guards; `apply` replays the record through `PromotionService` with the
//! real ports: the approval store and plan authors of the target site's PostgreSQL, the migration
//! audit chain of **both** sites, a rehearsal on a `pg_dump` clone of the target, the bundle
//! applier over the target's customisation records, and verification; `abort` drops leftover
//! clones. The environment identity is `spot_environments::EnvRef` (`promote/env.rs`).
use super::{CommandCtx, Exit};
use spot_cli::Result;

#[path = "promote/audit.rs"]
mod audit;
#[path = "promote/clone.rs"]
mod clone;
#[path = "promote/drive.rs"]
mod drive;
#[path = "promote/env.rs"]
mod env;
#[path = "promote/log.rs"]
mod log;
#[path = "promote/steps.rs"]
mod steps;
#[path = "promote/target.rs"]
mod target;

/// The command name.
pub const NAME: &str = "promote";
/// Printed when source and target are in different environment sets.
pub const ENVIRONMENT_MISMATCH: &str = "promote.environment_mismatch";
/// Printed when a production target has no rehearsal report younger than the policy limit.
pub const REHEARSAL_STALE: &str = "promote.rehearsal_stale";
/// Printed when production lacks two distinct approvers, one not the requester.
pub const APPROVALS_MISSING: &str = "promote.approvals_missing";

/// The command's arguments.
pub fn command() -> clap::Command {
    let site = |name: &'static str, help: &'static str| {
        clap::Arg::new(name)
            .long(name)
            .value_name("SITE")
            .required(true)
            .help(help)
    };
    let id = || {
        clap::Arg::new("id")
            .value_name("PROMOTION_ID")
            .required(true)
    };
    let as_user = |help: &'static str| {
        clap::Arg::new("as")
            .long("as")
            .value_name("USER")
            .required(true)
            .help(help)
    };
    clap::Command::new(NAME)
        .about("Promote a customisation bundle from one environment to the next")
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("request")
                .about("Ask for a promotion")
                .arg(site("from", "The source site"))
                .arg(site("to", "The target site"))
                .arg(
                    clap::Arg::new("bundle")
                        .long("bundle")
                        .value_name("FILE")
                        .required(true)
                        .help("The signed hand-over file (`spot bundle export`)"),
                )
                .arg(as_user("The requester")),
        )
        .subcommand(
            clap::Command::new("approve")
                .about("Approve a requested promotion")
                .arg(id())
                .arg(as_user("The approver"))
                .arg(
                    clap::Arg::new("role")
                        .long("role")
                        .value_name("ROLE")
                        .action(clap::ArgAction::Append)
                        .help("A role the approver holds (default: owner)"),
                )
                .arg(
                    clap::Arg::new("comment")
                        .long("comment")
                        .value_name("TEXT")
                        .help("Why (recorded in the audit)"),
                ),
        )
        .subcommand(
            clap::Command::new("status")
                .about("Show a promotion")
                .arg(id()),
        )
        .subcommand(
            clap::Command::new("apply")
                .about("Rehearse on a clone of the target, apply and verify")
                .arg(id())
                .arg(
                    clap::Arg::new("max-rehearsal-age-secs")
                        .long("max-rehearsal-age-secs")
                        .value_parser(clap::value_parser!(i64)),
                ),
        )
        .subcommand(
            clap::Command::new("abort")
                .about("Abort; drops the rehearsal clone")
                .arg(id()),
        )
}

/// Which environment set and role a site belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Env {
    /// The environment set (the pipeline the sites belong to).
    pub set: String,
    /// `dev`, `test` or `prod`.
    pub role: String,
}

/// Refuses a promotion whose two ends are not in the same environment set, whose target is not
/// the next role after the source (dev to test, test to prod), or whose target is the same site.
///
/// # Errors
///
/// A message starting with [`ENVIRONMENT_MISMATCH`].
pub fn check_environments(from: &Env, to: &Env) -> std::result::Result<(), String> {
    let next = |role: &str| match role {
        "dev" => Some("test"),
        "test" => Some("prod"),
        _ => None,
    };
    if from.set != to.set {
        return Err(format!(
            "{ENVIRONMENT_MISMATCH}: the sites are in different environment sets"
        ));
    }
    if next(&from.role) != Some(to.role.as_str()) {
        return Err(format!(
            "{ENVIRONMENT_MISMATCH}: {} cannot be promoted to {}",
            from.role, to.role
        ));
    }
    Ok(())
}

/// Production needs two distinct approvers, at least one of whom is not the requester; any
/// environment refuses an approval by the requester alone.
///
/// # Errors
///
/// A message starting with [`APPROVALS_MISSING`].
pub fn check_approvals(
    target_role: &str,
    requester: &str,
    approvers: &[&str],
) -> std::result::Result<(), String> {
    let mut distinct: Vec<&str> = approvers.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let independent = distinct.iter().any(|a| *a != requester);
    let needed = if target_role == "prod" { 2 } else { 1 };
    if distinct.len() >= needed && independent {
        Ok(())
    } else {
        Err(format!(
            "{APPROVALS_MISSING}: {target_role} needs {needed} approver(s), one not the requester"
        ))
    }
}

/// A production target needs a rehearsal report no older than `max_age_secs`.
///
/// # Errors
///
/// A message starting with [`REHEARSAL_STALE`].
pub fn check_rehearsal(
    target_role: &str,
    rehearsed_at: Option<i64>,
    now: i64,
    max_age_secs: i64,
) -> std::result::Result<(), String> {
    if target_role != "prod" {
        return Ok(());
    }
    match rehearsed_at {
        Some(at) if now.saturating_sub(at) <= max_age_secs && at <= now => Ok(()),
        _ => Err(format!(
            "{REHEARSAL_STALE}: production needs a rehearsal on a clone of the target in the last {max_age_secs}s"
        )),
    }
}

/// The `pg_dump` directory the rehearsal would use (for tests that skip without it).
#[must_use]
pub fn pg_bin_for_tests() -> Option<std::path::PathBuf> {
    clone::pg_bin()
}

/// Runs a subcommand.
pub async fn run(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> Result<Exit> {
    match matches.subcommand() {
        Some(("request", sub)) => steps::request(sub, ctx).await,
        Some(("approve", sub)) => steps::approve(sub, ctx),
        Some(("status", sub)) => steps::status(sub, ctx),
        Some(("apply", sub)) => steps::apply(sub, ctx).await,
        Some(("abort", sub)) => steps::abort(sub, ctx).await,
        _ => {
            writeln!(
                ctx.err(),
                "usage: spot promote <request|approve|status|apply|abort> ..."
            )
            .map_err(|e| spot_cli::Error(e.to_string()))?;
            Ok(Exit::Usage)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(set: &str, role: &str) -> Env {
        Env {
            set: set.into(),
            role: role.into(),
        }
    }

    #[test]
    fn the_command_tree_has_the_five_steps() {
        let command = command();
        let names: Vec<_> = command
            .get_subcommands()
            .map(|c| c.get_name().to_owned())
            .collect();
        assert_eq!(names, ["request", "approve", "status", "apply", "abort"]);
    }

    #[test]
    fn only_the_next_environment_of_the_same_set_is_a_target() {
        assert!(check_environments(&env("a", "dev"), &env("a", "test")).is_ok());
        assert!(check_environments(&env("a", "test"), &env("a", "prod")).is_ok());
        assert!(check_environments(&env("a", "dev"), &env("a", "prod")).is_err());
        assert!(check_environments(&env("a", "dev"), &env("b", "test")).is_err());
        assert!(check_environments(&env("a", "prod"), &env("a", "dev")).is_err());
    }

    #[test]
    fn production_needs_two_approvers_one_not_the_requester() {
        assert!(check_approvals("test", "alice", &["bob"]).is_ok());
        assert!(check_approvals("test", "alice", &["alice"]).is_err());
        assert!(check_approvals("prod", "alice", &["bob"]).is_err());
        assert!(check_approvals("prod", "alice", &["bob", "bob"]).is_err());
        assert!(check_approvals("prod", "alice", &["alice", "bob"]).is_ok());
    }

    #[test]
    fn a_stale_or_missing_rehearsal_refuses_production_only() {
        assert!(check_rehearsal("test", None, 1_000, 60).is_ok());
        assert!(check_rehearsal("prod", Some(990), 1_000, 60).is_ok());
        assert!(check_rehearsal("prod", Some(900), 1_000, 60).is_err());
        assert!(check_rehearsal("prod", None, 1_000, 60).is_err());
        assert!(check_rehearsal("prod", Some(2_000), 1_000, 60).is_err());
    }
}
