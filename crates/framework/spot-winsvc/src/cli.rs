//! `spot-winsvc`'s command-line surface (brief: `install|uninstall|start|stop|run|upgrade
//! --config PATH`). Parsing is portable (clap, no Windows API), so the Linux test host
//! can check every subcommand's shape and the `--config`/`--from-staging` requirements.

use clap::Parser;
use std::path::PathBuf;

/// `spot-winsvc`: the Windows Service host for `spot serve` and the bundled PostgreSQL.
#[derive(Debug, Parser)]
#[command(name = "spot-winsvc", about = "Spotledger Windows Service host")]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// One `spot-winsvc` subcommand.
#[derive(Debug, clap::Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Registers the `SpotledgerServer` service (idempotent).
    Install {
        /// The service configuration file the registered service will run with.
        #[arg(long)]
        config: PathBuf,
    },
    /// Unregisters the `SpotledgerServer` service (idempotent).
    Uninstall,
    /// Starts the already-registered service through the Service Control Manager.
    Start,
    /// Stops the running service through the Service Control Manager.
    Stop,
    /// The SCM entry point: supervises PostgreSQL and `spot serve` in the foreground.
    /// Never run this directly in production; the SCM runs it.
    Run {
        /// The service configuration file.
        #[arg(long)]
        config: PathBuf,
    },
    /// Switches to a staged version (brief: "used by the updater"): stop, switch the
    /// pointer, plan-only migration check, start; rolls back on a failed health check.
    Upgrade {
        /// The staging directory containing the new version's files.
        #[arg(long = "from-staging")]
        from_staging: PathBuf,
        /// The service configuration file.
        #[arg(long)]
        config: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn run_requires_a_config_path() {
        let cli = Cli::try_parse_from(["spot-winsvc", "run", "--config", "C:\\spot.toml"])
            .expect("run --config parses");
        assert_eq!(
            cli.command,
            Command::Run {
                config: PathBuf::from("C:\\spot.toml")
            }
        );
    }

    #[test]
    fn run_without_config_is_a_usage_error() {
        Cli::try_parse_from(["spot-winsvc", "run"]).expect_err("missing --config");
    }

    #[test]
    fn install_requires_a_config_path() {
        let cli = Cli::try_parse_from(["spot-winsvc", "install", "--config", "spot.toml"])
            .expect("install --config parses");
        assert_eq!(
            cli.command,
            Command::Install {
                config: PathBuf::from("spot.toml")
            }
        );
    }

    #[test]
    fn uninstall_start_stop_take_no_arguments() {
        assert_eq!(
            Cli::try_parse_from(["spot-winsvc", "uninstall"])
                .expect("uninstall parses")
                .command,
            Command::Uninstall
        );
        assert_eq!(
            Cli::try_parse_from(["spot-winsvc", "start"])
                .expect("start parses")
                .command,
            Command::Start
        );
        assert_eq!(
            Cli::try_parse_from(["spot-winsvc", "stop"])
                .expect("stop parses")
                .command,
            Command::Stop
        );
    }

    #[test]
    fn upgrade_requires_from_staging_and_config() {
        let cli = Cli::try_parse_from([
            "spot-winsvc",
            "upgrade",
            "--from-staging",
            "C:\\staging\\1.2.0",
            "--config",
            "C:\\spot.toml",
        ])
        .expect("upgrade parses");
        assert_eq!(
            cli.command,
            Command::Upgrade {
                from_staging: PathBuf::from("C:\\staging\\1.2.0"),
                config: PathBuf::from("C:\\spot.toml"),
            }
        );
    }

    #[test]
    fn upgrade_without_from_staging_is_a_usage_error() {
        Cli::try_parse_from(["spot-winsvc", "upgrade", "--config", "spot.toml"])
            .expect_err("missing --from-staging");
    }

    #[test]
    fn an_unknown_subcommand_is_a_usage_error() {
        Cli::try_parse_from(["spot-winsvc", "frobnicate"]).expect_err("unknown subcommand");
    }
}
