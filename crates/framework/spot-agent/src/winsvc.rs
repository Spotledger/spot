//! The Windows service host (WP-ADM-05.T6): the same executable runs as the service
//! `SpotledgerAgent` (and installs the helper service `SpotledgerAgentPriv`).
//!
//! The module is always compiled; the parts that touch the Service Control Manager are
//! `cfg(windows)`. Everything that can be decided without Windows lives here as plain data
//! and functions so it is unit-tested on Linux: the service definitions, the exact `sc.exe`
//! command lists (installation is **idempotent**: a second install is a no-op, a changed
//! binary path updates the service), the binary-path safety check (**no binary in a
//! user-writable directory**), the control-event state machine (stop and shutdown drain
//! with a deadline), the pipe ACL and the command-line grammar.
//!
//! `sc.exe` is run argv-only (no shell, no prompts), so the install is silent. The SCM
//! dispatcher itself needs `windows-service` (cargo feature `scm`).
//!
//! # Exit codes of `spot-agent service ...`
//!
//! | code | meaning |
//! | --- | --- |
//! | 0 | done, or nothing to do (idempotent) |
//! | 1 | failed (an `sc.exe` call failed) |
//! | 2 | usage |
//! | 5 | access denied: run elevated |
//! | 10 | refused: the binary path is not safe (user-writable or not absolute) |

use std::time::Duration;

/// The agent service name.
pub const SERVICE_NAME: &str = "SpotledgerAgent";
/// The helper service name.
pub const PRIV_SERVICE_NAME: &str = "SpotledgerAgentPriv";
/// The virtual account the agent service runs as.
pub const SERVICE_ACCOUNT: &str = r"NT SERVICE\SpotledgerAgent";
/// How long a stop or shutdown waits for the running command before the service gives up.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(30);
/// Exit code: done or nothing to do.
pub const EXIT_OK: i32 = 0;
/// Exit code: failed.
pub const EXIT_FAILED: i32 = 1;
/// Exit code: usage.
pub const EXIT_USAGE: i32 = 2;
/// Exit code: access denied.
pub const EXIT_ACCESS_DENIED: i32 = 5;
/// Exit code: unsafe binary path.
pub const EXIT_UNSAFE_PATH: i32 = 10;

/// An action the SCM takes when the service fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Restart the service after `delay`.
    Restart(Duration),
}

/// One service to register.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceDef {
    /// The SCM name.
    pub name: &'static str,
    /// The display name.
    pub display_name: &'static str,
    /// The description.
    pub description: &'static str,
    /// The absolute path of the executable.
    pub binary_path: String,
    /// Arguments after the executable (the service-run subcommand).
    pub arguments: Vec<&'static str>,
    /// The account: `LocalSystem` or the virtual account.
    pub account: &'static str,
    /// Automatic start, delayed.
    pub delayed_auto_start: bool,
    /// What the SCM does after the first, second and later failures.
    pub recovery: Vec<RecoveryAction>,
}

impl ServiceDef {
    /// The agent service: delayed automatic start, the virtual account, restart after 5 s,
    /// 30 s and 60 s.
    #[must_use]
    pub fn agent(binary_path: impl Into<String>) -> Self {
        Self {
            name: SERVICE_NAME,
            display_name: "Spotledger Agent",
            description: "The Spotledger host agent (outbound-only management channel).",
            binary_path: binary_path.into(),
            arguments: vec!["service", "run"],
            account: SERVICE_ACCOUNT,
            delayed_auto_start: true,
            recovery: vec![
                RecoveryAction::Restart(Duration::from_secs(5)),
                RecoveryAction::Restart(Duration::from_secs(30)),
                RecoveryAction::Restart(Duration::from_secs(60)),
            ],
        }
    }

    /// The privileged helper service: LocalSystem, serving the named pipe whose ACL admits
    /// only the agent account (see `privilege::protocol::pipe_sddl`).
    #[must_use]
    pub fn helper(binary_path: impl Into<String>) -> Self {
        Self {
            name: PRIV_SERVICE_NAME,
            display_name: "Spotledger Agent Privileged Helper",
            description: "Runs the Spotledger agent's fixed privileged operations for the agent account only.",
            binary_path: binary_path.into(),
            arguments: vec!["service", "run", "--helper"],
            account: "LocalSystem",
            delayed_auto_start: true,
            recovery: vec![
                RecoveryAction::Restart(Duration::from_secs(5)),
                RecoveryAction::Restart(Duration::from_secs(30)),
            ],
        }
    }

    /// The `binPath=` value: the quoted executable followed by its arguments.
    #[must_use]
    pub fn bin_path_value(&self) -> String {
        let mut text = format!("\"{}\"", self.binary_path);
        for a in &self.arguments {
            text.push(' ');
            text.push_str(a);
        }
        text
    }
}

/// Why a binary path was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRefused {
    /// Not an absolute drive path.
    NotAbsolute,
    /// Contains `..`, a quote, a control character or an alternate-stream colon.
    Suspicious,
    /// Inside a directory ordinary users can write to.
    UserWritable,
}

const USER_WRITABLE_MARKERS: &[&str] = &[
    r"\users\",
    r"\documents and settings\",
    r"\temp\",
    r"\tmp\",
    r"\appdata\",
    r"\programdata\",
    r"\downloads\",
    r"\desktop\",
    r"\public\",
    r"\windows\tasks\",
    r"\windows\temp\",
];

/// Refuses a binary path in a user-writable directory (and anything not absolute or
/// suspicious). Only the machine-wide install roots pass: `Program Files`,
/// `Program Files (x86)` and the Windows directory, on any drive.
///
/// # Errors
/// [`PathRefused`].
pub fn check_binary_path(path: &str) -> Result<(), PathRefused> {
    let bytes = path.as_bytes();
    let absolute = bytes.len() > 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if !absolute {
        return Err(PathRefused::NotAbsolute);
    }
    if path.contains("..")
        || path.contains('"')
        || path.chars().any(char::is_control)
        || path[2..].contains(':')
    {
        return Err(PathRefused::Suspicious);
    }
    let lower = path.replace('/', "\\").to_ascii_lowercase();
    if USER_WRITABLE_MARKERS.iter().any(|m| lower[2..].contains(m)) {
        return Err(PathRefused::UserWritable);
    }
    let rest = &lower[2..];
    let allowed = rest.starts_with(r"\program files\")
        || rest.starts_with(r"\program files (x86)\")
        || rest.starts_with(r"\windows\");
    if allowed {
        Ok(())
    } else {
        Err(PathRefused::UserWritable)
    }
}

/// What the SCM says about an existing service (parsed from `sc qc`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Existing {
    /// The `BINARY_PATH_NAME` value.
    pub bin_path: String,
    /// The `SERVICE_START_NAME` value.
    pub account: String,
    /// Delayed automatic start.
    pub delayed_auto_start: bool,
}

/// Parses `sc qc <name>` output; `None` when it does not describe a service.
#[must_use]
pub fn parse_sc_qc(output: &str) -> Option<Existing> {
    let field = |key: &str| {
        output.lines().find_map(|l| {
            l.trim()
                .strip_prefix(key)
                .map(|r| r.trim_start_matches([' ', ':']).trim().to_owned())
        })
    };
    let bin_path = field("BINARY_PATH_NAME")?;
    let account = field("SERVICE_START_NAME").unwrap_or_default();
    let start = output
        .lines()
        .find(|l| l.trim().starts_with("START_TYPE"))
        .unwrap_or_default();
    Some(Existing {
        bin_path,
        account,
        delayed_auto_start: start.contains("DELAYED"),
    })
}

/// One `sc.exe` invocation (argv after the program).
pub type ScCommand = Vec<String>;

fn s(parts: &[&str]) -> ScCommand {
    parts.iter().map(|p| (*p).to_owned()).collect()
}

/// The recovery action list as `sc failure` wants it: `restart/5000/restart/30000/...`.
#[must_use]
pub fn failure_actions(def: &ServiceDef) -> String {
    def.recovery
        .iter()
        .map(|RecoveryAction::Restart(d)| format!("restart/{}", d.as_millis()))
        .collect::<Vec<_>>()
        .join("/")
}

fn config_commands(def: &ServiceDef, verb: &str) -> Vec<ScCommand> {
    let start = if def.delayed_auto_start {
        "delayed-auto"
    } else {
        "auto"
    };
    let mut create = vec![
        verb.to_owned(),
        def.name.to_owned(),
        "binPath=".to_owned(),
        def.bin_path_value(),
        "start=".to_owned(),
        start.to_owned(),
        "DisplayName=".to_owned(),
        def.display_name.to_owned(),
    ];
    if def.account != "LocalSystem" {
        create.push("obj=".to_owned());
        create.push(def.account.to_owned());
    } else if verb == "config" {
        create.push("obj=".to_owned());
        create.push("LocalSystem".to_owned());
    }
    vec![
        create,
        s(&["description", def.name, def.description]),
        vec![
            "failure".to_owned(),
            def.name.to_owned(),
            "reset=".to_owned(),
            "86400".to_owned(),
            "actions=".to_owned(),
            failure_actions(def),
        ],
        s(&["sidtype", def.name, "unrestricted"]),
    ]
}

/// The commands that bring the system to `def`: everything when the service is missing,
/// nothing when it already matches (a second install is a no-op), a `config` update when
/// the binary path or account differs.
#[must_use]
pub fn plan_install(existing: Option<&Existing>, def: &ServiceDef) -> Vec<ScCommand> {
    match existing {
        None => config_commands(def, "create"),
        Some(e)
            if e.bin_path.trim() == def.bin_path_value()
                && e.account.eq_ignore_ascii_case(def.account)
                && e.delayed_auto_start == def.delayed_auto_start =>
        {
            Vec::new()
        }
        Some(_) => config_commands(def, "config"),
    }
}

/// The commands that remove the service: stop (a no-op when stopped) then delete.
#[must_use]
pub fn plan_uninstall(existing: Option<&Existing>, name: &str) -> Vec<ScCommand> {
    match existing {
        None => Vec::new(),
        Some(_) => vec![s(&["stop", name]), s(&["delete", name])],
    }
}

/// A parsed `spot-agent service ...` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceCommand {
    /// `service install [--binary PATH]`.
    Install {
        /// Explicit binary path; the running executable's when `None`.
        binary: Option<String>,
    },
    /// `service uninstall`.
    Uninstall,
    /// `service run [--helper]`.
    Run {
        /// Run the privileged helper service instead of the agent.
        helper: bool,
    },
}

/// Parses the arguments after `service`.
///
/// # Errors
/// A usage message.
pub fn parse_service_args(args: &[String]) -> Result<ServiceCommand, String> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["install"] => Ok(ServiceCommand::Install { binary: None }),
        ["install", "--binary", path] => Ok(ServiceCommand::Install {
            binary: Some((*path).to_owned()),
        }),
        ["uninstall"] => Ok(ServiceCommand::Uninstall),
        ["run"] => Ok(ServiceCommand::Run { helper: false }),
        ["run", "--helper"] => Ok(ServiceCommand::Run { helper: true }),
        _ => Err(
            "usage: spot-agent service install [--binary PATH] | uninstall | run [--helper]"
                .to_owned(),
        ),
    }
}

/// An SCM control event, reduced to what the state machine cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlEvent {
    /// `SERVICE_CONTROL_STOP`.
    Stop,
    /// `SERVICE_CONTROL_SHUTDOWN`.
    Shutdown,
    /// `SERVICE_CONTROL_INTERROGATE`.
    Interrogate,
    /// Anything else.
    Other,
}

/// The service state as reported to the SCM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Running.
    Running,
    /// Stop requested: draining until the deadline.
    StopPending {
        /// Seconds the SCM may wait (the wait hint).
        wait_hint_secs: u64,
    },
    /// Stopped.
    Stopped,
}

/// What the control handler does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reaction {
    /// The new state.
    pub state: State,
    /// Trigger the agent's cooperative shutdown now.
    pub trigger_shutdown: bool,
    /// The control was understood.
    pub handled: bool,
}

/// The control-event state machine: stop and shutdown trigger the cooperative shutdown and
/// report `StopPending` with the drain deadline (plus 5 s of slack) as the wait hint.
#[must_use]
pub fn on_control(state: State, event: ControlEvent) -> Reaction {
    match (state, event) {
        (State::Running, ControlEvent::Stop | ControlEvent::Shutdown) => Reaction {
            state: State::StopPending {
                wait_hint_secs: DRAIN_DEADLINE.as_secs() + 5,
            },
            trigger_shutdown: true,
            handled: true,
        },
        (s, ControlEvent::Stop | ControlEvent::Shutdown | ControlEvent::Interrogate) => Reaction {
            state: s,
            trigger_shutdown: false,
            handled: true,
        },
        (s, ControlEvent::Other) => Reaction {
            state: s,
            trigger_shutdown: false,
            handled: false,
        },
    }
}

/// The SCM seam: one `sc.exe` call, argv only. The Windows build runs the real program;
/// tests use a fake, so the install, uninstall, start, stop and status paths are tested on
/// any host.
pub trait Scm {
    /// Runs `sc.exe args...` and returns its exit code and standard output.
    ///
    /// # Errors
    /// An I/O error when the program cannot be run.
    fn sc(&self, args: &[String]) -> std::io::Result<(i32, String)>;
}

/// `sc qc <name>` parsed; `None` when the service does not exist or the call failed.
pub fn query_with(scm: &dyn Scm, name: &str) -> Option<Existing> {
    let (code, text) = scm.sc(&["qc".to_owned(), name.to_owned()]).ok()?;
    if code == 0 { parse_sc_qc(&text) } else { None }
}

/// Runs the commands in order and stops at the first failure. `sc stop` of a stopped
/// service (1062) and `sc start` of a running one (1056) are fine.
pub fn run_all_with(scm: &dyn Scm, commands: &[ScCommand]) -> i32 {
    for command in commands {
        let verb = command.first().map(String::as_str);
        match scm.sc(command) {
            Ok((0, _)) => {}
            Ok((1062, _)) if verb == Some("stop") => {}
            Ok((1056, _)) if verb == Some("start") => {}
            Ok((5, _)) => return EXIT_ACCESS_DENIED,
            _ => return EXIT_FAILED,
        }
    }
    EXIT_OK
}

/// The commands that install (or update) the agent and helper services from `binary`,
/// given what the SCM already holds.
///
/// # Errors
/// [`PathRefused`] when `binary` is not a safe path.
pub fn plan_install_all(
    binary: &str,
    existing_agent: Option<&Existing>,
    existing_helper: Option<&Existing>,
) -> Result<Vec<ScCommand>, PathRefused> {
    check_binary_path(binary)?;
    let mut plan = plan_install(existing_agent, &ServiceDef::agent(binary));
    plan.extend(plan_install(existing_helper, &ServiceDef::helper(binary)));
    Ok(plan)
}

/// The commands that remove both services.
#[must_use]
pub fn plan_uninstall_all(
    existing_agent: Option<&Existing>,
    existing_helper: Option<&Existing>,
) -> Vec<ScCommand> {
    let mut plan = plan_uninstall(existing_agent, SERVICE_NAME);
    plan.extend(plan_uninstall(existing_helper, PRIV_SERVICE_NAME));
    plan
}

/// Installs (or updates) the agent and helper services from `binary` through `scm`.
pub fn install_with(scm: &dyn Scm, binary: &str) -> i32 {
    let existing_agent = query_with(scm, SERVICE_NAME);
    let existing_helper = query_with(scm, PRIV_SERVICE_NAME);
    match plan_install_all(binary, existing_agent.as_ref(), existing_helper.as_ref()) {
        Ok(plan) => run_all_with(scm, &plan),
        Err(_) => EXIT_UNSAFE_PATH,
    }
}

/// Removes both services through `scm`.
pub fn uninstall_with(scm: &dyn Scm) -> i32 {
    let existing_agent = query_with(scm, SERVICE_NAME);
    let existing_helper = query_with(scm, PRIV_SERVICE_NAME);
    run_all_with(
        scm,
        &plan_uninstall_all(existing_agent.as_ref(), existing_helper.as_ref()),
    )
}

/// Starts the agent service (the helper starts with the machine; the agent asks it).
pub fn start_with(scm: &dyn Scm) -> i32 {
    run_all_with(scm, &[vec!["start".to_owned(), SERVICE_NAME.to_owned()]])
}

/// Stops the agent service.
pub fn stop_with(scm: &dyn Scm) -> i32 {
    run_all_with(scm, &[vec!["stop".to_owned(), SERVICE_NAME.to_owned()]])
}

/// The service state word of `sc query` output (`RUNNING`, `STOPPED`, ...), if any.
#[must_use]
pub fn parse_sc_state(output: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.trim_start().starts_with("STATE"))
        .and_then(|line| line.split_whitespace().last())
        .map(str::to_owned)
}

/// The status line and exit code of the agent service: 0 when running, 3 when installed
/// but not running or not installed (the `systemctl status` convention).
pub fn status_with(scm: &dyn Scm) -> (i32, String) {
    match scm.sc(&["query".to_owned(), SERVICE_NAME.to_owned()]) {
        Ok((0, text)) => match parse_sc_state(&text) {
            Some(state) if state == "RUNNING" => (EXIT_OK, format!("{SERVICE_NAME}: running")),
            Some(state) => (3, format!("{SERVICE_NAME}: {}", state.to_ascii_lowercase())),
            None => (EXIT_FAILED, format!("{SERVICE_NAME}: state unknown")),
        },
        Ok((1060, _)) => (3, format!("{SERVICE_NAME}: not installed")),
        Ok((5, _)) => (EXIT_ACCESS_DENIED, "access denied: run elevated".to_owned()),
        _ => (
            EXIT_FAILED,
            format!("{SERVICE_NAME}: the SCM cannot be queried"),
        ),
    }
}

/// One `sc.exe` command as a line a person can read (and paste): arguments with a space or
/// a quote are wrapped in double quotes. For `--print`; nothing is ever run through a shell.
#[must_use]
pub fn render_sc(command: &ScCommand) -> String {
    let mut line = String::from("sc.exe");
    for argument in command {
        line.push(' ');
        if argument.contains([' ', '"']) {
            line.push('"');
            line.push_str(&argument.replace('"', "\\\""));
            line.push('"');
        } else {
            line.push_str(argument);
        }
    }
    line
}

#[cfg(windows)]
pub use scm::{RealScm, install, run_service, start, status, stop, uninstall};

/// The SCM-facing implementation (Windows only).
#[cfg(windows)]
mod scm {
    use std::process::Command;

    use super::{
        EXIT_FAILED, Scm, install_with, start_with, status_with, stop_with, uninstall_with,
    };

    /// The real `sc.exe`.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct RealScm;

    impl Scm for RealScm {
        fn sc(&self, args: &[String]) -> std::io::Result<(i32, String)> {
            let out = Command::new("sc.exe").args(args).output()?;
            Ok((
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            ))
        }
    }

    /// Installs (or updates) the agent and helper services from `binary`.
    #[must_use]
    pub fn install(binary: &str) -> i32 {
        install_with(&RealScm, binary)
    }

    /// Removes both services.
    #[must_use]
    pub fn uninstall() -> i32 {
        uninstall_with(&RealScm)
    }

    /// Starts the agent service.
    #[must_use]
    pub fn start() -> i32 {
        start_with(&RealScm)
    }

    /// Stops the agent service.
    #[must_use]
    pub fn stop() -> i32 {
        stop_with(&RealScm)
    }

    /// The status line and exit code of the agent service.
    #[must_use]
    pub fn status() -> (i32, String) {
        status_with(&RealScm)
    }

    /// Runs as the SCM service (needs the `scm` feature; without it the SCM would kill the
    /// process for not answering).
    #[cfg(feature = "scm")]
    pub fn run_service(helper: bool) -> i32 {
        host::dispatch(helper)
    }

    /// Without the `scm` feature the service host is not built in.
    #[cfg(not(feature = "scm"))]
    #[must_use]
    pub fn run_service(_helper: bool) -> i32 {
        EXIT_FAILED
    }

    #[cfg(feature = "scm")]
    mod host {
        use std::time::Duration;

        use windows_service::service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        };
        use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
        use windows_service::{define_windows_service, service_dispatcher};

        use super::super::{ControlEvent, EXIT_FAILED, EXIT_OK, SERVICE_NAME, State, on_control};
        use crate::config::LogLevel;
        use crate::logging::Logger;
        use crate::run::{Shutdown, run};
        use crate::state::{StateDir, default_state_dir};

        define_windows_service!(ffi_service_main, service_main);

        pub fn dispatch(helper: bool) -> i32 {
            if helper {
                // The named-pipe server of the privileged helper needs a Windows pipe
                // wrapper that is not part of this task.
                return EXIT_FAILED;
            }
            match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
                Ok(()) => EXIT_OK,
                Err(_) => EXIT_FAILED,
            }
        }

        fn service_main(_arguments: Vec<std::ffi::OsString>) {
            let _ = run_agent_service();
        }

        fn status(
            state: ServiceState,
            accept: ServiceControlAccept,
            wait_hint: Duration,
            code: u32,
        ) -> ServiceStatus {
            ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: accept,
                exit_code: ServiceExitCode::Win32(code),
                checkpoint: 0,
                wait_hint,
                process_id: None,
            }
        }

        fn run_agent_service() -> Result<(), windows_service::Error> {
            let shutdown = Shutdown::new();
            let trigger = shutdown.clone();
            let handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
                let event = match control {
                    ServiceControl::Stop => ControlEvent::Stop,
                    ServiceControl::Shutdown => ControlEvent::Shutdown,
                    ServiceControl::Interrogate => ControlEvent::Interrogate,
                    _ => ControlEvent::Other,
                };
                let reaction = on_control(State::Running, event);
                if reaction.trigger_shutdown {
                    trigger.trigger();
                }
                if reaction.handled {
                    ServiceControlHandlerResult::NoError
                } else {
                    ServiceControlHandlerResult::NotImplemented
                }
            };
            let handle = service_control_handler::register(SERVICE_NAME, handler)?;
            handle.set_service_status(status(
                ServiceState::Running,
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                Duration::default(),
                0,
            ))?;
            let state = StateDir::new(default_state_dir());
            let logger = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(state.log_path())
            {
                Ok(f) => Logger::new(LogLevel::Info, Box::new(f)),
                Err(_) => Logger::new(LogLevel::Info, Box::new(std::io::sink())),
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|d| i64::try_from(d.as_secs()).ok())
                .unwrap_or(0);
            let code = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => u32::from(rt.block_on(run(&state, logger, shutdown, now)).is_err()),
                Err(_) => 1,
            };
            handle.set_service_status(status(
                ServiceState::StopPending,
                ServiceControlAccept::empty(),
                Duration::from_secs(5),
                0,
            ))?;
            handle.set_service_status(status(
                ServiceState::Stopped,
                ServiceControlAccept::empty(),
                Duration::default(),
                code,
            ))?;
            Ok(())
        }
    }
}
