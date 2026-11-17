//! [`PrivilegedRunner`]: the command runner the Linux module crates run through in the
//! agent. A command that matches a privileged template goes to the helper
//! (`sudo -n <helper> run <name>`, parameters on stdin); a command on the small read-only
//! allow-list runs directly as the agent user; **everything else is refused** (the agent
//! never runs an arbitrary command, privileged or not).

use std::sync::Arc;

use spot_agent_linux::{CommandOutput as LinuxOutput, CommandRequest, LinuxError};
use spot_agent_linux_sys::error::{COMMAND_TIMEOUT, SysError, SysResult};
use spot_agent_linux_sys::{CommandOutput as SysOutput, CommandSpec};

use super::HELPER_PATH;
use super::exec::{ChildResult, ChildRunner, ChildSpec, StdChildRunner};
use super::helper::HelperReply;
use super::templates::route;

/// Read-only programs the agent user may run directly, with the sub-commands allowed.
/// Options (arguments starting with `-`) before the sub-command are skipped.
pub const DIRECT_READ_ONLY: &[(&str, &str, &[&str])] = &[
    (
        "systemctl",
        "/usr/bin/systemctl",
        &[
            "show",
            "is-active",
            "is-enabled",
            "list-units",
            "list-unit-files",
        ],
    ),
    ("id", "/usr/bin/id", &[]),
    ("getent", "/usr/bin/getent", &[]),
    ("uname", "/usr/bin/uname", &[]),
    ("hostnamectl", "/usr/bin/hostnamectl", &["status"]),
];

/// Refusal reasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    /// The command is neither a template nor on the read-only list.
    NotAllowed,
    /// The helper could not be used (sudo refused, no reply).
    HelperUnavailable,
    /// The program could not be started.
    Spawn,
}

impl Denied {
    /// A stable code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NotAllowed => "agent.privilege_refused",
            Self::HelperUnavailable => "agent.privilege_unavailable",
            Self::Spawn => "agent.privilege_spawn_failed",
        }
    }
}

/// The runner.
pub struct PrivilegedRunner {
    exec: Arc<dyn ChildRunner>,
    sudo: String,
    helper: String,
}

impl Default for PrivilegedRunner {
    fn default() -> Self {
        Self::new(Arc::new(StdChildRunner))
    }
}

impl PrivilegedRunner {
    /// A runner over `exec` using the installed helper path.
    #[must_use]
    pub fn new(exec: Arc<dyn ChildRunner>) -> Self {
        Self {
            exec,
            sudo: "/usr/bin/sudo".to_owned(),
            helper: HELPER_PATH.to_owned(),
        }
    }

    fn direct_program(program: &str, args: &[String]) -> Option<&'static str> {
        let base = program.rsplit('/').next().unwrap_or(program);
        let (_, path, subs) = DIRECT_READ_ONLY.iter().find(|(n, _, _)| *n == base)?;
        if program.contains('/') && program != *path {
            return None;
        }
        if subs.is_empty() {
            return Some(path);
        }
        let sub = args.iter().find(|a| !a.starts_with('-'))?;
        subs.contains(&sub.as_str()).then_some(path)
    }

    /// Routes one command: helper, direct, or refusal.
    ///
    /// # Errors
    /// [`Denied`].
    pub fn execute(
        &self,
        program: &str,
        args: &[String],
        timeout_secs: u64,
        output_cap: usize,
    ) -> Result<ChildResult, Denied> {
        if let Some((template, params)) = route(program, args) {
            let stdin = serde_json::json!({ "params": params }).to_string() + "\n";
            let spec = ChildSpec {
                program: self.sudo.clone(),
                args: vec![
                    "-n".to_owned(),
                    self.helper.clone(),
                    "run".to_owned(),
                    template.name.to_owned(),
                ],
                timeout_secs: template.timeout_secs.min(timeout_secs.max(1)),
                output_cap: output_cap.max(65_536).saturating_add(4096),
                stdin: Some(stdin.into_bytes()),
            };
            let raw = self.exec.run(&spec).map_err(|_| Denied::Spawn)?;
            let reply: HelperReply =
                serde_json::from_str(raw.stdout.trim()).map_err(|_| Denied::HelperUnavailable)?;
            if !reply.ok {
                return Err(Denied::NotAllowed);
            }
            return Ok(ChildResult {
                status: reply.status,
                stdout: reply.stdout,
                stderr: reply.stderr,
                timed_out: reply.timed_out,
                truncated: reply.truncated || raw.truncated,
            });
        }
        let path = Self::direct_program(program, args).ok_or(Denied::NotAllowed)?;
        self.exec
            .run(&ChildSpec {
                program: path.to_owned(),
                args: args.to_vec(),
                timeout_secs,
                output_cap,
                stdin: None,
            })
            .map_err(|_| Denied::Spawn)
    }
}

impl spot_agent_linux::CommandRunner for PrivilegedRunner {
    fn run(&self, request: &CommandRequest) -> Result<LinuxOutput, LinuxError> {
        let result = self
            .execute(
                &request.program,
                &request.args,
                request.timeout_secs,
                request.output_cap,
            )
            .map_err(|denied| LinuxError::CommandSpawn {
                program: request.program.clone(),
                detail: denied.code().to_owned(),
            })?;
        if result.timed_out {
            return Err(LinuxError::CommandTimeout {
                program: request.program.clone(),
                timeout_secs: request.timeout_secs,
            });
        }
        Ok(LinuxOutput {
            status: result.status,
            stdout: result.stdout,
            stderr: result.stderr,
            timed_out: false,
            truncated: result.truncated,
        })
    }
}

impl spot_agent_linux_sys::CommandRunner for PrivilegedRunner {
    fn run(&self, spec: &CommandSpec) -> SysResult<SysOutput> {
        let result = self
            .execute(spec.program(), spec.arguments(), spec.timeout(), spec.cap())
            .map_err(|denied| SysError::refused(denied.code()))?;
        if result.timed_out {
            return Err(SysError::new(
                COMMAND_TIMEOUT,
                "the command ran past its timeout and was killed",
            ));
        }
        Ok(SysOutput::new(result.status, result.stdout, result.stderr).truncated(result.truncated))
    }
}
