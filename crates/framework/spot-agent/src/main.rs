//! `spot-agent`: the host agent executable.

mod revert;

use std::process::ExitCode;

use spot_agent::cli::{Command, USAGE, parse};
use spot_agent::config::LogLevel;
use spot_agent::enrol::{EnrolInput, PlatformWire, enrol, rotate_key};
use spot_agent::error::AgentError;
use spot_agent::logging::{Logger, build_redactor, install_panic_hook};
use spot_agent::run::{Shutdown, run, watch_signals};
use spot_agent::state::{StateDir, default_state_dir};

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

fn platform() -> Result<PlatformWire, AgentError> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "windows" => "windows",
        _ => return Err(AgentError::Usage("unsupported platform".to_owned())),
    };
    Ok(PlatformWire {
        os: os.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        agent_version: spot_agent::VERSION.to_owned(),
    })
}

fn open_logger(state: &StateDir, level: LogLevel, token: Option<&str>) -> Logger {
    let sink: Box<dyn std::io::Write + Send> = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.log_path())
    {
        Ok(file) => Box::new(file),
        Err(_) => Box::new(std::io::stderr()),
    };
    let secrets: Vec<(&str, &str)> = token
        .map(|t| vec![("enrolment_token", t)])
        .unwrap_or_default();
    Logger::with_redactor(level, sink, build_redactor(&secrets))
}

/// Start-up network recovery (Linux): deals with a network change a previous run left
/// pending, before the channel client starts and before any plan is accepted. A failure is
/// logged and remembered for `status`; it never stops the agent (the independent timer is
/// the safety net).
fn recover_network_at_start(state: &StateDir, logger: &Logger) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let exe = match revert::current_executable() {
        Ok(exe) => exe,
        Err(message) => {
            logger.warn("network.recover", &message);
            return;
        }
    };
    let handler = revert::build_handler(
        state.root(),
        &exe,
        std::sync::Arc::new(spot_agent::privilege::PrivilegedRunner::default()),
        std::sync::Arc::new(spot_agent_linux_sys::StdFileSystem::new()),
        std::sync::Arc::new(spot_core::SystemClock::new()),
    );
    let identity = spot_agent_enroll::identity::AgentIdentity::load(state.key_path()).ok();
    let agent_id = identity
        .as_ref()
        .map(|i| i.agent_id.clone())
        .unwrap_or_default();
    let audit = identity.as_ref().and_then(|i| {
        spot_agent::audit::LocalAuditLog::open(state.audit_path(), i.agent_id.clone()).ok()
    });
    let report =
        revert::startup_recovery(&handler, state, audit.as_ref(), &agent_id, logger, now());
    if !report.ok {
        eprintln!("{}", report.summary);
    }
}

fn real_main() -> Result<(), AgentError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = parse(&args, &|p| std::fs::read_to_string(p).ok())?;
    let state = StateDir::new(cli.state_dir.clone().unwrap_or_else(default_state_dir));
    match cli.command {
        Command::Version => {
            println!("spot-agent {}", spot_agent::VERSION);
            Ok(())
        }
        Command::Enrol {
            gateway,
            token,
            ca,
            key_store,
            allow_software_keys,
        } => {
            state.create()?;
            let logger = open_logger(&state, LogLevel::Info, Some(&token));
            let transport = spot_agent::channel::enrol_gateway(
                &gateway,
                ca.as_deref().map(std::path::Path::new),
            )?;
            let summary = enrol(
                &EnrolInput {
                    state: &state,
                    gateway_url: &gateway,
                    ca: ca.as_deref(),
                    token: &token,
                    key_store,
                    platform: platform()?,
                    now: now(),
                    allow_software_keys,
                },
                &*transport,
                &logger,
            )?;
            println!(
                "enrolled as {} (key {}, {} trusted administrator key(s))",
                summary.agent_id, summary.fingerprint, summary.trusted_keys
            );
            Ok(())
        }
        Command::RotateKey { token, key_store } => {
            let logger = open_logger(&state, LogLevel::Info, Some(&token));
            let text = std::fs::read_to_string(state.config_path())
                .map_err(|_| AgentError::NotEnrolled)?;
            let config = spot_agent::config::AgentConfig::parse(&text)?;
            let transport = spot_agent::channel::enrol_gateway(
                &config.gateway,
                config.ca.as_deref().map(std::path::Path::new),
            )?;
            let summary = rotate_key(
                &EnrolInput {
                    state: &state,
                    gateway_url: &config.gateway,
                    ca: config.ca.as_deref(),
                    token: &token,
                    key_store,
                    platform: platform()?,
                    now: now(),
                    allow_software_keys: false,
                },
                &*transport,
                &logger,
            )?;
            println!(
                "key rotated for {} (new key {})",
                summary.agent_id, summary.fingerprint
            );
            Ok(())
        }
        Command::Probe => spot_agent::update::probe::probe(&state, now()),
        Command::Service(request) => {
            let executable = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(str::to_owned))
                .ok_or_else(|| {
                    AgentError::Usage("cannot find the running executable".to_owned())
                })?;
            let install_root = std::fs::read_to_string(state.config_path())
                .ok()
                .and_then(|text| spot_agent::config::AgentConfig::parse(&text).ok())
                .map_or_else(
                    || spot_agent::config::default_install_root().to_owned(),
                    |config| config.install_root().to_owned(),
                );
            let env = spot_agent::service_cmd::ServiceEnv {
                executable,
                state_dir: state.root().to_string_lossy().into_owned(),
                install_root,
            };
            let outcome = spot_agent::service_cmd::run_here(&request, &env);
            for line in &outcome.lines {
                println!("{line}");
            }
            if outcome.code != 0 {
                std::process::exit(outcome.code);
            }
            Ok(())
        }
        Command::AuditVerify { file, agent_id } => {
            let path = file.map_or_else(|| state.audit_path(), std::path::PathBuf::from);
            let site = match agent_id {
                Some(id) => id,
                None => spot_agent_enroll::identity::AgentIdentity::load(state.key_path())
                    .map(|identity| identity.agent_id)
                    .map_err(|_| {
                        AgentError::Usage(
                            "this host is not enrolled: pass --agent-id ID".to_owned(),
                        )
                    })?,
            };
            let count = spot_agent::audit::verify_file(&path, &site)?;
            println!("audit log ok: {count} entries");
            Ok(())
        }
        Command::Status => {
            if !state.is_enrolled() {
                println!("not enrolled");
                return Err(AgentError::NotEnrolled);
            }
            let identity = spot_agent_enroll::identity::AgentIdentity::load(state.key_path())
                .map_err(|_| {
                    AgentError::ConfigInvalid("the enrolment record is malformed".to_owned())
                })?;
            println!("enrolled: yes\nagent id: {}", identity.agent_id);
            if let Ok(text) = std::fs::read_to_string(state.config_path())
                && let Ok(config) = spot_agent::config::AgentConfig::parse(&text)
            {
                println!(
                    "gateway: {}\nallowed modules: {}",
                    config.gateway,
                    config.allowed_modules.join(",")
                );
            }
            if let Ok(trust) = spot_agent::trust::TrustFile::load(&state.trust_path()) {
                println!(
                    "administrator keys: {}",
                    if trust.software_keys_allowed {
                        "software keys accepted (chosen at enrolment)"
                    } else {
                        "hardware-backed keys required"
                    }
                );
            }
            for line in spot_agent::channel::status_lines(&state) {
                println!("{line}");
            }
            for line in revert::recovery_status_lines(&state) {
                println!("{line}");
            }
            Ok(())
        }
        Command::Run => {
            let level = std::fs::read_to_string(state.config_path())
                .ok()
                .and_then(|t| spot_agent::config::AgentConfig::parse(&t).ok())
                .map_or(LogLevel::Info, |c| c.log_level);
            let logger = open_logger(&state, level, None);
            install_panic_hook(logger.clone());
            recover_network_at_start(&state, &logger);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| AgentError::StateIo("cannot start the runtime".to_owned()))?;
            let shutdown = Shutdown::new();
            runtime.block_on(async {
                watch_signals(&shutdown);
                run(&state, logger, shutdown, now()).await
            })
        }
    }
}

/// `network-revert <id>` is run by the independent systemd timer: no enrolment, no gateway,
/// no state checks. `None` when the command line is another command.
fn network_revert_exit(args: &[String]) -> Option<ExitCode> {
    let (state_dir, rest) = revert::split_revert_args(args)?;
    let exit = revert::network_revert_main(state_dir, rest);
    if exit.code == 0 {
        println!("{}", exit.line);
    } else {
        eprintln!("{}", exit.line);
    }
    Some(ExitCode::from(exit.code))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = network_revert_exit(&args) {
        return code;
    }
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            if matches!(error, AgentError::Usage(_)) {
                eprintln!("{USAGE}");
            }
            ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(1))
        }
    }
}
