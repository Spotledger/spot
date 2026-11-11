//! `spot-winsvc`: the Windows Service host binary (brief: `install|uninstall|start|stop|
//! run|upgrade --config PATH`). Builds on every platform (`cargo clippy --all-targets`
//! runs on Linux CI); `install`/`uninstall`/`start`/`stop` need the Service Control
//! Manager and report exit code 2 ("prerequisite missing") on a non-Windows host, `run`
//! drives the orchestrator in the foreground there (useful for local development), and
//! `upgrade`'s pointer-switch logic is fully portable.

use clap::Parser;
use semver::Version;
use spot_winsvc::cli::{Cli, Command};
use spot_winsvc::config::ServiceConfig;
use spot_winsvc::error::WinsvcError;
use spot_winsvc::upgrade::{RealUpgradeHost, run_upgrade};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("spot-winsvc: {err}");
            let code = err.exit_code();
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
    }
}

fn dispatch(command: Command) -> Result<(), WinsvcError> {
    match command {
        Command::Install { config } => install(&config),
        Command::Uninstall => uninstall(),
        Command::Start => start(),
        Command::Stop => stop(),
        Command::Run { config } => run(&config),
        Command::Upgrade {
            from_staging,
            config,
        } => upgrade(&from_staging, &config),
    }
}

#[cfg(windows)]
fn install(config_path: &Path) -> Result<(), WinsvcError> {
    let binary_path =
        std::env::current_exe().map_err(|e| WinsvcError::io("locating the running binary", &e))?;
    spot_winsvc::service::install::install(&binary_path, config_path)
}

#[cfg(not(windows))]
fn install(_config_path: &Path) -> Result<(), WinsvcError> {
    Err(WinsvcError::prerequisite(
        "the Windows Service Control Manager",
    ))
}

#[cfg(windows)]
fn uninstall() -> Result<(), WinsvcError> {
    spot_winsvc::service::install::uninstall()
}

#[cfg(not(windows))]
fn uninstall() -> Result<(), WinsvcError> {
    Err(WinsvcError::prerequisite(
        "the Windows Service Control Manager",
    ))
}

#[cfg(windows)]
fn start() -> Result<(), WinsvcError> {
    spot_winsvc::service::install::start()
}

#[cfg(not(windows))]
fn start() -> Result<(), WinsvcError> {
    Err(WinsvcError::prerequisite(
        "the Windows Service Control Manager",
    ))
}

#[cfg(windows)]
fn stop() -> Result<(), WinsvcError> {
    spot_winsvc::service::install::stop()
}

#[cfg(not(windows))]
fn stop() -> Result<(), WinsvcError> {
    Err(WinsvcError::prerequisite(
        "the Windows Service Control Manager",
    ))
}

#[cfg(windows)]
fn run(config_path: &Path) -> Result<(), WinsvcError> {
    spot_winsvc::service::dispatch::run(config_path.to_path_buf())
}

/// A non-Windows "run": there is no SCM, so this drives the orchestrator directly in the
/// foreground until Ctrl-C, for local development against the bundled PostgreSQL and a
/// real `spot serve` binary.
#[cfg(not(windows))]
fn run(config_path: &Path) -> Result<(), WinsvcError> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| WinsvcError::io("starting the async runtime", &e))?;
    runtime.block_on(async move {
        let stop_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag_for_signal = stop_flag.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                stop_flag_for_signal.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
        spot_winsvc::run::run_foreground(
            config_path,
            || eprintln!("spot-winsvc: SERVICE_RUNNING (health check answered)"),
            move || stop_flag.load(std::sync::atomic::Ordering::SeqCst),
        )
        .await
    })
}

/// Parses the target version from `--from-staging DIR`'s own directory name (the
/// updater's staging layout names it after the version it holds, `spot-updater`'s
/// `Layout::staging_path`).
fn version_from_staging_dir(from_staging: &Path) -> Result<Version, WinsvcError> {
    let name = from_staging
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| WinsvcError::usage("--from-staging must name a directory"))?;
    Version::parse(name)
        .map_err(|e| WinsvcError::usage(format!("--from-staging directory is not a version: {e}")))
}

fn upgrade(from_staging: &Path, config_path: &Path) -> Result<(), WinsvcError> {
    if !from_staging.is_dir() {
        return Err(WinsvcError::prerequisite(format!(
            "staging directory {}",
            from_staging.display()
        )));
    }
    let target = version_from_staging_dir(from_staging)?;
    let config = ServiceConfig::load(config_path)
        .map_err(|e| WinsvcError::failed(format!("loading service config: {e}")))?;
    let layout = spot_updater::Layout::new(config.update_root.clone());
    let host = RealUpgradeHost { config };
    let outcome = run_upgrade(&layout, &target, &host)?;
    match outcome {
        spot_winsvc::upgrade::UpgradeOutcome::Applied { version } => {
            println!("spot-winsvc: upgraded to {version}");
            Ok(())
        }
        spot_winsvc::upgrade::UpgradeOutcome::Refused { reason } => {
            Err(WinsvcError::failed(format!("upgrade refused: {reason}")))
        }
        spot_winsvc::upgrade::UpgradeOutcome::RolledBack { restored } => {
            Err(WinsvcError::failed(format!(
                "upgrade failed its health check and rolled back to {}",
                restored
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "no previous version".to_string())
            )))
        }
    }
}
