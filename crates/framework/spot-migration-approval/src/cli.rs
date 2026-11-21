//! The `plan show`, `approve`, `reject` and `status` subcommands as a library module.
//!
//! They return structured output ([`Output`]) and never print or exit. The `spot-cli` binary
//! does not exist in the workspace yet, so this module exposes [`register`] over a small
//! [`CommandSet`] trait and leaves the one wiring line to the lead.

use serde_json::{Value, json};
use spot_migration_plan::ReleasePlan;

use crate::approval::{ApprovalStore, Checks, Identity};
use crate::error::ApprovalError;
use crate::service::{Act, ApprovalService, AuditSink, AuthoriseError, Status};

/// The registry a host CLI implements to receive these subcommands.
pub trait CommandSet {
    /// Registers one subcommand path (for example `"plan show"`) with a one-line description.
    fn register(&mut self, path: &str, about: &str);
}

/// Registers the four subcommands.
pub fn register(set: &mut dyn CommandSet) {
    set.register(
        "plan show",
        "Show a release plan with its digest and approval status",
    );
    set.register(
        "approve",
        "Approve exactly this plan (--role R [--comment C] [--signature HEX])",
    );
    set.register("reject", "Reject this plan (--role R [--comment C])");
    set.register(
        "status",
        "Show who approved, who rejected and whether apply may run",
    );
}

/// A parsed subcommand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `plan show`.
    PlanShow,
    /// `approve --role R [--comment C] [--signature S]`.
    Approve {
        /// The approver's role.
        role: String,
        /// The reason.
        comment: String,
        /// A hardware signature (hex).
        signature: Option<String>,
    },
    /// `reject --role R [--comment C]`.
    Reject {
        /// The approver's role.
        role: String,
        /// The reason.
        comment: String,
    },
    /// `status`.
    Status,
}

fn bad(message: impl Into<String>) -> ApprovalError {
    ApprovalError::BadCommand {
        message: message.into(),
    }
}

impl Command {
    /// Parses the words after the program name.
    ///
    /// # Errors
    /// [`ApprovalError::BadCommand`] for an unknown subcommand, an unknown or valueless flag,
    /// or a missing `--role`.
    pub fn parse(args: &[&str]) -> Result<Self, ApprovalError> {
        let (name, rest) = match args {
            ["plan", "show", rest @ ..] => ("plan show", rest),
            [name @ ("approve" | "reject" | "status"), rest @ ..] => (*name, rest),
            _ => return Err(bad("expected: plan show | approve | reject | status")),
        };
        let mut role = None;
        let mut comment = String::new();
        let mut signature = None;
        let mut it = rest.iter();
        while let Some(flag) = it.next() {
            let value = |v: Option<&&str>| {
                v.map(|s| (*s).to_owned())
                    .ok_or_else(|| bad(format!("{flag} needs a value")))
            };
            match (*flag, name) {
                ("--role", "approve" | "reject") => role = Some(value(it.next())?),
                ("--comment", "approve" | "reject") => comment = value(it.next())?,
                ("--signature", "approve") => signature = Some(value(it.next())?),
                _ => return Err(bad(format!("unknown option {flag} for {name}"))),
            }
        }
        let need_role = || role.clone().ok_or_else(|| bad("--role is required"));
        match name {
            "plan show" => Ok(Self::PlanShow),
            "status" => Ok(Self::Status),
            "approve" => Ok(Self::Approve {
                role: need_role()?,
                comment,
                signature,
            }),
            _ => Ok(Self::Reject {
                role: need_role()?,
                comment,
            }),
        }
    }
}

/// The structured result of a subcommand.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    /// Whether the command succeeded.
    pub ok: bool,
    /// The stable error code when it did not.
    pub code: Option<String>,
    /// The result document: the status on success, `{ "message": ... }` on failure.
    pub data: Value,
}

impl Output {
    fn failure(code: &str, message: &str) -> Self {
        Self {
            ok: false,
            code: Some(code.to_owned()),
            data: json!({ "message": message }),
        }
    }

    fn from_error(error: &ApprovalError) -> Self {
        Self::failure(error.code(), &error.to_string())
    }

    fn success(status: &Status, extra: Value) -> Self {
        Self {
            ok: true,
            code: None,
            data: json!({ "status": status, "result": extra }),
        }
    }
}

/// Runs one subcommand as the verified `identity` against `plan`.
///
/// The command line cannot supply an identity: the host's authentication does, and `--role` only
/// says which of the identity's roles to act in.
pub fn run<S: ApprovalStore, A: AuditSink>(
    service: &ApprovalService<S, A>,
    plan: &ReleasePlan,
    command: &Command,
    identity: &Identity,
    now: i64,
    checks: &Checks<'_>,
) -> Output {
    let actor = identity.subject.as_str();
    let outcome = match command {
        Command::PlanShow => service
            .record_shown(plan, actor, now)
            .and_then(|()| service.status(plan, now, checks))
            .map(|s| (s, json!({ "plan": plan.render_text() }))),
        Command::Status => service
            .record_status_viewed(plan, actor, now)
            .and_then(|()| service.status(plan, now, checks))
            .map(|s| (s, Value::Null)),
        Command::Approve {
            role,
            comment,
            signature,
        } => {
            let act = Act {
                identity: identity.clone(),
                role,
                comment,
                signature: signature.clone(),
                now,
            };
            service.approve(plan, &act, checks).and_then(|a| {
                Ok((
                    service.status(plan, now, checks)?,
                    json!({ "recorded": a.decision }),
                ))
            })
        }
        Command::Reject { role, comment } => {
            let act = Act {
                identity: identity.clone(),
                role,
                comment,
                signature: None,
                now,
            };
            service.reject(plan, &act, checks).and_then(|a| {
                Ok((
                    service.status(plan, now, checks)?,
                    json!({ "recorded": a.decision }),
                ))
            })
        }
    };
    match outcome {
        Ok((status, extra)) => Output::success(&status, extra),
        Err(error) => Output::from_error(&error),
    }
}

/// Asks for the apply ticket as `actor`, as the apply engine's front door does; the refusal
/// comes back as structured output.
pub fn authorise<S: ApprovalStore, A: AuditSink>(
    service: &ApprovalService<S, A>,
    plan: &ReleasePlan,
    actor: &str,
    now: i64,
    checks: &Checks<'_>,
) -> Output {
    match service.authorise_apply(plan, actor, now, checks) {
        Ok(ticket) => Output {
            ok: true,
            code: None,
            data: json!({ "digest": ticket.digest().as_hex(), "approvers": ticket.approvers() }),
        },
        Err(AuthoriseError::Refused(r)) => Output::failure(r.code(), &r.to_string()),
        Err(AuthoriseError::Approval(e)) => Output::from_error(&e),
    }
}
