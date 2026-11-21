//! `spot access emergency`: the operator's side of emergency access (WP-IAM-08.T14). Guide:
//! `docs/running-spot/emergency_access.md`.
//!
//! - `request`: raise a request for the acting user (a role, a reason, an approver, minutes);
//! - `list`, `status <id>`: what exists and where each grant is;
//! - `approve <id>`, `reject <id>`: the approver's decision (`approve` may carry an override of a
//!   segregation-of-duties conflict: a reason and a mitigating control, when the policy allows);
//! - `end <id>`: end an active session now;
//! - `review <id>`: a controller's `--confirm` or `--flag` of an ended session.
//!
//! Every command takes `--site` and `--as <user name>` (who acts), and `--json`. Each goes through
//! the same `Emergency` the server runs, so no command bypasses the policy: it asks for what the
//! policy allows and is refused with the policy's stable code otherwise. What needs the running
//! server (starting the approval Case, the segregation-of-duties gate, archiving the recording)
//! is done by the server's sweep within seconds of the command. Exit codes: 0 done, 1 usage,
//! 2 refused (nothing changed), 3 failed. Nothing printed holds a secret: ids, roles, phases and
//! codes.
use super::{CommandCtx, Exit};
use spot_cli::Result;
use spot_cli::services::emergency_access::model::{EmergencyError, GrantView, NewRequest};
use spot_cli::services::emergency_access::ops::Offline;
use spot_iam_governance::{MitigatingControl, OverrideSubmission};

pub const NAME: &str = "access";

fn text(name: &'static str, long: &'static str, help: &'static str) -> clap::Arg {
    clap::Arg::new(name)
        .long(long)
        .value_name("TEXT")
        .help(help)
}

fn common(command: clap::Command) -> clap::Command {
    command
        .arg(
            clap::Arg::new("site")
                .long("site")
                .value_name("SITE")
                .required(true)
                .help("The site to act on"),
        )
        .arg(
            clap::Arg::new("as")
                .long("as")
                .value_name("USER")
                .required(true)
                .help("The user name of the person acting"),
        )
        .arg(
            clap::Arg::new("json")
                .long("json")
                .action(clap::ArgAction::SetTrue)
                .help("Print JSON"),
        )
}

fn id_arg() -> clap::Arg {
    clap::Arg::new("id")
        .value_name("GRANT")
        .required(true)
        .help("The grant id")
}

pub fn command() -> clap::Command {
    clap::Command::new(NAME)
        .about("Access governance")
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("emergency")
                .about("Emergency access (the firefighter pattern)")
                .subcommand_required(true)
                .subcommand(common(
                    clap::Command::new("request")
                        .about("Request an elevated role for yourself, with a reason")
                        .arg(text("role", "role", "The elevated role").required(true))
                        .arg(
                            text("reason", "reason", "Why emergency access is needed")
                                .required(true),
                        )
                        .arg(
                            text(
                                "approver",
                                "approver",
                                "Who approves (a user id or role:<name>)",
                            )
                            .required(true),
                        )
                        .arg(
                            clap::Arg::new("minutes")
                                .long("minutes")
                                .value_name("MINUTES")
                                .value_parser(clap::value_parser!(u64).range(1..=1440))
                                .required(true)
                                .help("How long the session may run"),
                        ),
                ))
                .subcommand(common(
                    clap::Command::new("list").about("Every grant and where it is"),
                ))
                .subcommand(common(
                    clap::Command::new("status")
                        .about("One grant")
                        .arg(id_arg()),
                ))
                .subcommand(common(
                    clap::Command::new("approve")
                        .about("Approve a request (as its approver)")
                        .arg(id_arg())
                        .arg(text(
                            "override_reason",
                            "override-reason",
                            "Why a segregation-of-duties conflict is tolerated",
                        ))
                        .arg(text(
                            "control_id",
                            "control-id",
                            "The mitigating control's id",
                        ))
                        .arg(text(
                            "control_text",
                            "control-text",
                            "The mitigating control, in words",
                        )),
                ))
                .subcommand(common(
                    clap::Command::new("reject")
                        .about("Reject a request (as its approver)")
                        .arg(id_arg())
                        .arg(text("reason", "reason", "Why").required(true)),
                ))
                .subcommand(common(
                    clap::Command::new("end")
                        .about("End an active session now")
                        .arg(id_arg()),
                ))
                .subcommand(common(
                    clap::Command::new("review")
                        .about("Confirm or flag an ended session (as its controller)")
                        .arg(id_arg())
                        .arg(
                            clap::Arg::new("confirm")
                                .long("confirm")
                                .action(clap::ArgAction::SetTrue)
                                .conflicts_with("flag"),
                        )
                        .arg(
                            clap::Arg::new("flag")
                                .long("flag")
                                .action(clap::ArgAction::SetTrue)
                                .conflicts_with("confirm"),
                        )
                        .arg(text(
                            "note",
                            "note",
                            "The reviewer's note (required with --flag)",
                        )),
                )),
        )
}

fn show(ctx: &mut CommandCtx, json: bool, view: &GrantView) -> Result<()> {
    if json {
        let line = serde_json::to_string(view)
            .map_err(|_| spot_cli::Error("the grant could not be printed".into()))?;
        writeln!(ctx.out(), "{line}")?;
    } else {
        let ends = view
            .ends_at
            .map_or(String::new(), |t| format!(" ends_at={t}"));
        let review = view.review.as_ref().map_or(String::new(), |r| {
            format!(" review={} controller={}", r.state, r.controller)
        });
        writeln!(
            ctx.out(),
            "{} user={} role={} phase={}{ends}{review} recording={}{}",
            view.id,
            view.user,
            view.role,
            view.phase,
            view.recording,
            view.blocked
                .as_ref()
                .map_or(String::new(), |b| format!(" blocked={b}")),
        )?;
    }
    Ok(())
}

/// A refusal by the policy is exit 2; a part that failed is exit 3.
fn exit_of(ctx: &mut CommandCtx, error: &EmergencyError) -> Result<Exit> {
    writeln!(ctx.err(), "{}", error.code)?;
    let failed = matches!(
        error.code,
        "emergency_access_host.unavailable"
            | "emergency_access_host.write_failed"
            | "emergency_access_host.audit_unavailable"
            | "emergency_access_host.state_invalid"
            | "emergency_access_host.state_version"
    );
    Ok(if failed { Exit::Failed } else { Exit::Refused })
}

pub async fn run(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> Result<Exit> {
    let Some(("emergency", emergency)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot access emergency <request|list|status|approve|reject|end|review>"
        )?;
        return Ok(Exit::Usage);
    };
    let Some((verb, sub)) = emergency.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot access emergency <request|list|status|approve|reject|end|review>"
        )?;
        return Ok(Exit::Usage);
    };
    let arg = |name: &str| sub.get_one::<String>(name).cloned().unwrap_or_default();
    let (site, acting, json) = (arg("site"), arg("as"), sub.get_flag("json"));
    let offline = match Offline::open(ctx.root(), &site, ctx.clock().clone()).await {
        Ok(offline) => offline,
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            return Ok(Exit::Refused);
        }
    };
    let by = match offline.user_id(&acting).await {
        Ok(id) => id,
        Err(error) => {
            writeln!(ctx.err(), "{error}")?;
            return Ok(Exit::Refused);
        }
    };
    let em = &offline.emergency;
    let outcome = match verb {
        "request" => {
            let minutes = sub.get_one::<u64>("minutes").copied().unwrap_or(0);
            em.request(
                &by,
                NewRequest {
                    role: arg("role"),
                    reason: arg("reason"),
                    approver: arg("approver"),
                    minutes,
                },
            )
            .await
        }
        "status" => em.status(&arg("id")).await,
        "approve" => {
            let over = if sub.get_one::<String>("override_reason").is_some() {
                Some(OverrideSubmission::new(
                    &arg("override_reason"),
                    MitigatingControl::new(&arg("control_id"), &arg("control_text")),
                ))
            } else {
                None
            };
            em.approve(&arg("id"), &by, over).await
        }
        "reject" => em.reject(&arg("id"), &by, &arg("reason")).await,
        "end" => em.end(&arg("id"), &by).await,
        "review" => {
            if !sub.get_flag("confirm") && !sub.get_flag("flag") {
                writeln!(ctx.err(), "say --confirm or --flag")?;
                return Ok(Exit::Usage);
            }
            em.review(&arg("id"), &by, sub.get_flag("confirm"), &arg("note"))
                .await
        }
        "list" => {
            return match em.list().await {
                Ok((views, unreadable)) => {
                    for view in &views {
                        show(ctx, json, view)?;
                    }
                    if unreadable > 0 {
                        writeln!(ctx.err(), "{unreadable} stored record(s) could not be read")?;
                        return Ok(Exit::Failed);
                    }
                    Ok(Exit::Ok)
                }
                Err(error) => exit_of(ctx, &error),
            };
        }
        _ => {
            writeln!(ctx.err(), "unknown emergency command `{verb}`")?;
            return Ok(Exit::Usage);
        }
    };
    match outcome {
        Ok(view) => {
            show(ctx, json, &view)?;
            Ok(Exit::Ok)
        }
        Err(error) => exit_of(ctx, &error),
    }
}
