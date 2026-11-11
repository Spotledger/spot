//! The `spot-winsvc` service configuration file (`--config PATH`): everything the
//! orchestrator needs to supervise PostgreSQL and `spot serve` is read from one TOML file,
//! never from the registry or the command line (brief: "Silent semantics").

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// PostgreSQL layout settings read from the config file.
#[derive(Debug, Clone, Deserialize)]
pub struct PgConfig {
    /// Directory holding `postgres`, `initdb`, `pg_ctl`.
    pub bin_dir: PathBuf,
    /// The PostgreSQL data directory.
    pub data_dir: PathBuf,
    /// Directory for the rotating PostgreSQL log.
    pub log_dir: PathBuf,
    /// TCP port postgres listens on (loopback only; brief "Security notes").
    #[serde(default = "default_pg_port")]
    pub port: u16,
}

fn default_pg_port() -> u16 {
    5433
}

/// The `spot serve` child process settings.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    /// Path to the `spot` binary (under `%ProgramFiles%\Spotledger`).
    pub spot_binary: PathBuf,
    /// `--root` passed to `spot serve` (the site root).
    pub site_root: PathBuf,
    /// The site name `spot migrate` plans against during an upgrade's compatibility
    /// check ([`crate::upgrade::RealUpgradeHost::check_migration_plan`]'s `--site`).
    /// `None` when the install manages exactly one site and `spot migrate` can infer it.
    #[serde(default)]
    pub site: Option<String>,
    /// Host and port of `GET /api/health/ready` (brief: "report `SERVICE_RUNNING` only
    /// when `/api/health/ready` answers").
    #[serde(default = "default_health_addr")]
    pub health_addr: String,
    /// How long `spot serve` may take to drain on stop, in milliseconds (never a float —
    /// ADR-12 — and never the wall clock; the orchestrator is handed the deadline).
    #[serde(default = "default_shutdown_grace_ms")]
    pub shutdown_grace_ms: u64,
}

fn default_health_addr() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_shutdown_grace_ms() -> u64 {
    30_000
}

impl AppConfig {
    /// [`AppConfig::shutdown_grace_ms`] as a [`Duration`], plus the margin the brief asks
    /// the service to wait beyond the host's own drain budget before it moves on.
    #[must_use]
    pub fn shutdown_grace(&self, margin: Duration) -> Duration {
        Duration::from_millis(self.shutdown_grace_ms) + margin
    }
}

/// The whole `spot-winsvc` configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    /// PostgreSQL settings.
    pub postgres: PgConfig,
    /// `spot serve` settings.
    pub app: AppConfig,
    /// The `spot-updater` install root (`current`/`previous` pointers, `versions/`,
    /// `staging/`): defaults to `%ProgramData%\Spotledger` (brief: "`upgrade` entry
    /// point ... used by the updater").
    #[serde(default = "default_update_root")]
    pub update_root: PathBuf,
}

fn default_update_root() -> PathBuf {
    crate::dirs::program_data_dir()
}

/// A validation problem with a config file: human words, never the file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigProblem(pub String);

impl std::fmt::Display for ConfigProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl ServiceConfig {
    /// Parses `text` as TOML and validates it. Never touches the filesystem; the caller
    /// reads the file ([`load`](ServiceConfig::load)).
    ///
    /// # Errors
    /// One [`ConfigProblem`] per malformed or missing field, plus every semantic problem
    /// [`validate`](ServiceConfig::validate) finds.
    pub fn parse(text: &str) -> Result<Self, Vec<ConfigProblem>> {
        let config: ServiceConfig =
            toml::from_str(text).map_err(|e| vec![ConfigProblem(format!("invalid TOML: {e}"))])?;
        let problems = config.validate();
        if problems.is_empty() {
            Ok(config)
        } else {
            Err(problems)
        }
    }

    /// Reads and parses the config file at `path`.
    ///
    /// # Errors
    /// `Err(Ok(problems))` for a parse/validation problem, `Err(Err(io_error))` when the
    /// file cannot be read.
    pub fn load(path: &Path) -> Result<Self, ConfigLoadError> {
        let text = std::fs::read_to_string(path).map_err(ConfigLoadError::Io)?;
        Self::parse(&text).map_err(ConfigLoadError::Invalid)
    }

    /// Every semantic problem with an otherwise well-formed config: empty paths, a port
    /// of zero, a health address that does not parse, a grace of zero.
    #[must_use]
    pub fn validate(&self) -> Vec<ConfigProblem> {
        let mut problems = Vec::new();
        if self.postgres.bin_dir.as_os_str().is_empty() {
            problems.push(ConfigProblem("postgres.bin_dir must not be empty".into()));
        }
        if self.postgres.data_dir.as_os_str().is_empty() {
            problems.push(ConfigProblem("postgres.data_dir must not be empty".into()));
        }
        if self.postgres.log_dir.as_os_str().is_empty() {
            problems.push(ConfigProblem("postgres.log_dir must not be empty".into()));
        }
        if self.postgres.port == 0 {
            problems.push(ConfigProblem("postgres.port must not be 0".into()));
        }
        if self.app.spot_binary.as_os_str().is_empty() {
            problems.push(ConfigProblem("app.spot_binary must not be empty".into()));
        }
        if self.app.site_root.as_os_str().is_empty() {
            problems.push(ConfigProblem("app.site_root must not be empty".into()));
        }
        if self
            .app
            .health_addr
            .parse::<std::net::SocketAddr>()
            .is_err()
        {
            problems.push(ConfigProblem(
                "app.health_addr is not a host:port address".into(),
            ));
        }
        if self.app.shutdown_grace_ms == 0 {
            problems.push(ConfigProblem("app.shutdown_grace_ms must not be 0".into()));
        }
        problems
    }
}

/// Why [`ServiceConfig::load`] failed.
#[derive(Debug)]
pub enum ConfigLoadError {
    /// The file could not be read.
    Io(std::io::Error),
    /// The file was read but is not a valid config.
    Invalid(Vec<ConfigProblem>),
}

impl std::fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigLoadError::Io(e) => write!(f, "reading service config: {e}"),
            ConfigLoadError::Invalid(problems) => {
                write!(f, "invalid service config: ")?;
                for (i, p) in problems.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{p}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigLoadError {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const VALID: &str = r#"
        [postgres]
        bin_dir = "/opt/Spotledger/pgsql/bin"
        data_dir = "/var/lib/Spotledger/pgdata"
        log_dir = "/var/lib/Spotledger/pglogs"
        port = 5433

        [app]
        spot_binary = "/opt/Spotledger/spot"
        site_root = "/var/lib/Spotledger/config"
        health_addr = "127.0.0.1:8080"
        shutdown_grace_ms = 15000
    "#;

    #[test]
    fn a_well_formed_config_parses_with_no_problems() {
        let config = ServiceConfig::parse(VALID).expect("valid config");
        assert_eq!(config.postgres.port, 5433);
        assert_eq!(config.app.shutdown_grace_ms, 15_000);
    }

    #[test]
    fn defaults_fill_in_port_and_health_addr_and_grace() {
        let text = r#"
            [postgres]
            bin_dir = "/opt/Spotledger/pgsql/bin"
            data_dir = "/var/lib/Spotledger/pgdata"
            log_dir = "/var/lib/Spotledger/pglogs"

            [app]
            spot_binary = "/opt/Spotledger/spot"
            site_root = "/var/lib/Spotledger/config"
        "#;
        let config = ServiceConfig::parse(text).expect("valid config with defaults");
        assert_eq!(config.postgres.port, 5433);
        assert_eq!(config.app.health_addr, "127.0.0.1:8080");
        assert_eq!(config.app.shutdown_grace_ms, 30_000);
    }

    #[test]
    fn a_zero_port_is_rejected() {
        let text = VALID.replace("port = 5433", "port = 0");
        let problems = ServiceConfig::parse(&text).expect_err("zero port is invalid");
        assert!(problems.iter().any(|p| p.0.contains("postgres.port")));
    }

    #[test]
    fn an_unparseable_health_address_is_rejected() {
        let text = VALID.replace("127.0.0.1:8080", "not-an-address");
        let problems = ServiceConfig::parse(&text).expect_err("bad health_addr is invalid");
        assert!(problems.iter().any(|p| p.0.contains("health_addr")));
    }

    #[test]
    fn a_zero_shutdown_grace_is_rejected() {
        let text = VALID.replace("shutdown_grace_ms = 15000", "shutdown_grace_ms = 0");
        let problems = ServiceConfig::parse(&text).expect_err("zero grace is invalid");
        assert!(problems.iter().any(|p| p.0.contains("shutdown_grace_ms")));
    }

    #[test]
    fn malformed_toml_is_one_problem_naming_the_syntax_error() {
        let problems = ServiceConfig::parse("not valid toml {{{").expect_err("malformed");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].0.contains("invalid TOML"));
    }

    #[test]
    fn shutdown_grace_adds_the_margin() {
        let config = ServiceConfig::parse(VALID).expect("valid config");
        let grace = config.app.shutdown_grace(Duration::from_secs(5));
        assert_eq!(grace, Duration::from_millis(20_000));
    }

    #[test]
    fn load_reports_a_missing_file_as_io_not_invalid() {
        let err = ServiceConfig::load(Path::new("/nonexistent/spot-winsvc.toml"))
            .expect_err("missing file");
        assert!(matches!(err, ConfigLoadError::Io(_)));
    }
}
