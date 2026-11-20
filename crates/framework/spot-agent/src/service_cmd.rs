//! `spot-agent service install|uninstall|start|stop|status` (and `run`, the Windows service
//! entry): the grammar, and the dispatch to the Linux systemd unit ([`crate::systemd`]) or
//! the Windows service module ([`crate::winsvc`]).
//!
//! Both platform paths exist in every build and take their machine seam as an argument
//! ([`systemd::Host`], [`winsvc::Scm`]), so the Windows install runs against a fake SCM on
//! Linux and the Linux unit against a fake host. `--print` never touches the machine: it
//! shows the files and commands the real run would use (as if nothing were installed yet).
//!
//! Exit codes: 0 done; 1 failed; 2 usage; 3 `status` of a service that is not running;
//! 5 access denied (Windows); 10 the binary path is not safe.

use crate::error::AgentError;
use crate::systemd::{self, UnitRefused, UnitSettings};
use crate::winsvc::{self, EXIT_OK, EXIT_UNSAFE_PATH, EXIT_USAGE, Scm};

/// What a `service` command does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    /// Install and enable (Linux) or create the services (Windows).
    Install,
    /// Remove the service; keeps the account and the state.
    Uninstall,
    /// Start it.
    Start,
    /// Stop it.
    Stop,
    /// Show whether it runs.
    Status,
    /// The Windows service entry (the SCM starts the executable with this).
    Run {
        /// Run the privileged helper service instead of the agent.
        helper: bool,
    },
}

/// A parsed `service` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceRequest {
    /// What to do.
    pub action: ServiceAction,
    /// Show the plan, change nothing.
    pub print: bool,
    /// Explicit executable path; the running executable when `None`.
    pub binary: Option<String>,
    /// The Linux account; `spot-agent` when `None`.
    pub user: Option<String>,
    /// Install so `sudo` works for the privileged helper (Linux).
    pub privileged_helper: bool,
}

fn usage(text: &str) -> AgentError {
    AgentError::Usage(text.to_owned())
}

/// The usage line of the `service` commands.
pub const SERVICE_USAGE: &str = "service install [--binary PATH] [--user NAME] [--privileged-helper] [--print] | uninstall [--print] | start [--print] | stop [--print] | status [--print] | run [--helper]";

/// Parses the arguments after `service`.
///
/// # Errors
/// [`AgentError::Usage`].
pub fn parse_service_request(args: &[String]) -> Result<ServiceRequest, AgentError> {
    let (verb, flags) = args
        .split_first()
        .ok_or_else(|| usage("service needs install, uninstall, start, stop or status"))?;
    let mut request = ServiceRequest {
        action: match verb.as_str() {
            "install" => ServiceAction::Install,
            "uninstall" => ServiceAction::Uninstall,
            "start" => ServiceAction::Start,
            "stop" => ServiceAction::Stop,
            "status" => ServiceAction::Status,
            "run" => ServiceAction::Run { helper: false },
            _ => return Err(usage("unknown service command")),
        },
        print: false,
        binary: None,
        user: None,
        privileged_helper: false,
    };
    let install = request.action == ServiceAction::Install;
    let mut iter = flags.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--print" if !matches!(request.action, ServiceAction::Run { .. }) => {
                request.print = true;
            }
            "--helper" if matches!(request.action, ServiceAction::Run { .. }) => {
                request.action = ServiceAction::Run { helper: true };
            }
            "--privileged-helper" if install => request.privileged_helper = true,
            "--binary" if install => {
                request.binary = Some(
                    iter.next()
                        .ok_or_else(|| usage("--binary needs a value"))?
                        .clone(),
                );
            }
            "--user" if install => {
                request.user = Some(
                    iter.next()
                        .ok_or_else(|| usage("--user needs a value"))?
                        .clone(),
                );
            }
            _ => return Err(usage("unknown or misplaced service option")),
        }
    }
    Ok(request)
}

/// What a `service` command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The process exit code.
    pub code: i32,
    /// Lines for standard output.
    pub lines: Vec<String>,
}

impl Outcome {
    fn new(code: i32, lines: Vec<String>) -> Self {
        Self { code, lines }
    }

    fn refused(code: i32, text: &str) -> Self {
        Self::new(code, vec![format!("refused: {text}")])
    }
}

/// The facts about this host a command needs, injected so tests choose them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceEnv {
    /// The running executable (the default `--binary`).
    pub executable: String,
    /// The state directory the service uses.
    pub state_dir: String,
    /// The install root the self-update writes to.
    pub install_root: String,
}

fn refusal_outcome(refused: &UnitRefused) -> Outcome {
    match refused {
        UnitRefused::Binary => Outcome::refused(
            EXIT_UNSAFE_PATH,
            "the binary path is not safe (use an absolute path of plain characters outside user-writable directories)",
        ),
        UnitRefused::User => Outcome::refused(EXIT_USAGE, "the account name is not valid"),
        UnitRefused::Directory => Outcome::refused(
            EXIT_USAGE,
            "the state directory and the install root must be plain absolute paths",
        ),
    }
}

/// Runs a request on Linux through `host`.
pub fn run_linux(request: &ServiceRequest, env: &ServiceEnv, host: &dyn systemd::Host) -> Outcome {
    let settings = UnitSettings {
        binary: request
            .binary
            .clone()
            .unwrap_or_else(|| env.executable.clone()),
        user: request
            .user
            .clone()
            .unwrap_or_else(|| systemd::DEFAULT_USER.to_owned()),
        state_dir: env.state_dir.clone(),
        install_root: env.install_root.clone(),
        privileged_helper: request.privileged_helper,
    };
    // `--print` shows the plan for a machine where nothing is installed yet.
    let have = if request.print {
        systemd::Existing::default()
    } else {
        systemd::existing(host, &settings.user)
    };
    let steps = match request.action {
        ServiceAction::Install => match systemd::plan_install(&settings, &have) {
            Ok(steps) => steps,
            Err(refused) => return refusal_outcome(&refused),
        },
        ServiceAction::Uninstall => {
            if request.print {
                systemd::plan_uninstall(&systemd::Existing {
                    unit: Some(String::new()),
                    user_exists: true,
                })
            } else {
                systemd::plan_uninstall(&have)
            }
        }
        ServiceAction::Start => systemd::plan_control("start"),
        ServiceAction::Stop => systemd::plan_control("stop"),
        ServiceAction::Status => systemd::plan_status(),
        ServiceAction::Run { .. } => {
            return Outcome::new(
                EXIT_USAGE,
                vec![
                    "`service run` is the Windows service entry; on Linux systemd runs `spot-agent run`"
                        .to_owned(),
                ],
            );
        }
    };
    if request.print {
        return Outcome::new(EXIT_OK, steps.iter().map(systemd::render_step).collect());
    }
    if steps.is_empty() {
        return Outcome::new(EXIT_OK, vec!["nothing to do: not installed".to_owned()]);
    }
    let code = systemd::execute(host, &steps);
    let line = match (request.action, code) {
        (ServiceAction::Install, 0) => {
            "installed and enabled spot-agent.service; start it with `spot-agent service start`"
        }
        (ServiceAction::Uninstall, 0) => {
            "removed spot-agent.service; the account and the state directory were kept"
        }
        (ServiceAction::Start, 0) => "started spot-agent.service",
        (ServiceAction::Stop, 0) => "stopped spot-agent.service",
        (ServiceAction::Status, 0) => "spot-agent.service is active",
        (ServiceAction::Status, _) => "spot-agent.service is not active (see the status above)",
        (_, _) => "failed: a command above did not succeed",
    };
    Outcome::new(code, vec![line.to_owned()])
}

/// Runs a request on Windows through `scm`.
pub fn run_windows(request: &ServiceRequest, env: &ServiceEnv, scm: &dyn Scm) -> Outcome {
    let binary = request
        .binary
        .clone()
        .unwrap_or_else(|| env.executable.clone());
    let printed = |plan: Vec<winsvc::ScCommand>| {
        Outcome::new(EXIT_OK, plan.iter().map(winsvc::render_sc).collect())
    };
    let named = |verb: &str| vec![verb.to_owned(), winsvc::SERVICE_NAME.to_owned()];
    match request.action {
        ServiceAction::Install if request.print => {
            match winsvc::plan_install_all(&binary, None, None) {
                Ok(plan) => printed(plan),
                Err(_) => Outcome::refused(EXIT_UNSAFE_PATH, "the binary path is not safe"),
            }
        }
        ServiceAction::Install => match winsvc::install_with(scm, &binary) {
            EXIT_UNSAFE_PATH => Outcome::refused(EXIT_UNSAFE_PATH, "the binary path is not safe"),
            code => Outcome::new(
                code,
                vec![if code == EXIT_OK {
                    "installed SpotledgerAgent and SpotledgerAgentPriv".to_owned()
                } else {
                    "failed: an sc.exe call did not succeed (run elevated)".to_owned()
                }],
            ),
        },
        ServiceAction::Uninstall if request.print => {
            let present = winsvc::Existing {
                bin_path: String::new(),
                account: String::new(),
                delayed_auto_start: false,
            };
            printed(winsvc::plan_uninstall_all(Some(&present), Some(&present)))
        }
        ServiceAction::Uninstall => {
            let code = winsvc::uninstall_with(scm);
            Outcome::new(code, vec![format!("uninstall finished with code {code}")])
        }
        ServiceAction::Start if request.print => printed(vec![named("start")]),
        ServiceAction::Stop if request.print => printed(vec![named("stop")]),
        ServiceAction::Status if request.print => printed(vec![named("query")]),
        ServiceAction::Start => {
            let code = winsvc::start_with(scm);
            Outcome::new(code, vec![format!("start finished with code {code}")])
        }
        ServiceAction::Stop => {
            let code = winsvc::stop_with(scm);
            Outcome::new(code, vec![format!("stop finished with code {code}")])
        }
        ServiceAction::Status => {
            let (code, line) = winsvc::status_with(scm);
            Outcome::new(code, vec![line])
        }
        ServiceAction::Run { helper } => {
            #[cfg(windows)]
            {
                Outcome::new(winsvc::run_service(helper), Vec::new())
            }
            #[cfg(not(windows))]
            {
                let _ = helper;
                Outcome::new(
                    winsvc::EXIT_FAILED,
                    vec!["`service run` works only on Windows".to_owned()],
                )
            }
        }
    }
}

/// Runs a request on the platform this binary was built for.
#[must_use]
pub fn run_here(request: &ServiceRequest, env: &ServiceEnv) -> Outcome {
    #[cfg(windows)]
    {
        run_windows(request, env, &winsvc::RealScm)
    }
    #[cfg(not(windows))]
    {
        run_linux(request, env, &systemd::SystemHost)
    }
}
