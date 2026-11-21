//! The helper's logic (`spot-agent-priv`), separated from `main` so it is testable with a
//! fake [`ChildRunner`].
//!
//! `run <template>` reads one JSON line `{"params":[...]}` from stdin, validates it against
//! the template **again**, runs the argv built from the template and prints one JSON reply
//! line. Unknown names, wrong counts and bad values are refused; nothing else is accepted.

use serde::{Deserialize, Serialize};
use spot_redact::{RedactionPolicy, Redactor};

use super::exec::{ChildRunner, ChildSpec};
use super::sudoers::sudoers_dropin;
use super::templates::{Refused, find};

/// The stdin request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperRequest {
    /// The parameters, in slot order.
    pub params: Vec<String>,
}

/// The stdout reply line.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelperReply {
    /// The helper accepted and ran the operation.
    pub ok: bool,
    /// Refusal code when `ok` is false.
    pub code: Option<String>,
    /// Child exit status.
    pub status: i32,
    /// Killed after the timeout.
    pub timed_out: bool,
    /// Output hit the cap.
    pub truncated: bool,
    /// Redacted stdout.
    pub stdout: String,
    /// Redacted stderr.
    pub stderr: String,
}

impl HelperReply {
    fn refused(code: &str) -> Self {
        Self {
            ok: false,
            code: Some(code.to_owned()),
            status: -1,
            timed_out: false,
            truncated: false,
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

/// Maximum accepted stdin, bytes.
pub const MAX_STDIN: usize = 16 * 1024;

/// Handles one invocation. Returns the process exit code and the stdout text.
#[must_use]
pub fn run_helper(args: &[String], stdin: &str, runner: &dyn ChildRunner) -> (i32, String) {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--print-sudoers", "--user", user] => match sudoers_dropin(user, super::HELPER_PATH) {
            Ok(text) => (0, text),
            Err(()) => (2, String::new()),
        },
        ["run", name] => {
            let reply = run_template(name, stdin, runner);
            let code = if reply.ok { 0 } else { 3 };
            let mut line = serde_json::to_string(&reply).unwrap_or_default();
            line.push('\n');
            (code, line)
        }
        _ => (2, String::new()),
    }
}

fn run_template(name: &str, stdin: &str, runner: &dyn ChildRunner) -> HelperReply {
    let Some(template) = find(name) else {
        return HelperReply::refused(Refused::UnknownOperation.code());
    };
    if stdin.len() > MAX_STDIN {
        return HelperReply::refused(Refused::ParamCount.code());
    }
    let Ok(request) = serde_json::from_str::<HelperRequest>(stdin.trim()) else {
        return HelperReply::refused(Refused::ParamCount.code());
    };
    let argv = match template.build(&request.params) {
        Ok(argv) => argv,
        Err(refused) => return HelperReply::refused(refused.code()),
    };
    let spec = ChildSpec {
        program: template.program.to_owned(),
        args: argv,
        timeout_secs: template.timeout_secs,
        output_cap: template.output_cap,
        stdin: None,
    };
    match runner.run(&spec) {
        Err(_) => HelperReply::refused("agent.privilege_spawn_failed"),
        Ok(result) => {
            let redactor = Redactor::from_policy(&RedactionPolicy::default()).ok();
            let clean = |s: &str| {
                redactor.as_ref().map_or_else(
                    || String::from("[redacted]"),
                    |r| r.redact_str(s).into_owned(),
                )
            };
            HelperReply {
                ok: true,
                code: None,
                status: result.status,
                timed_out: result.timed_out,
                truncated: result.truncated,
                stdout: clean(&result.stdout),
                stderr: clean(&result.stderr),
            }
        }
    }
}
