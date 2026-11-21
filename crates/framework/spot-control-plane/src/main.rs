//! `spot-control-plane`: `run --config FILE`, `migrate --config FILE`, `check --config FILE`,
//! `version`.
//!
//! `main` installs the providers of the modules before the service is built
//! (`spot_control_plane::wiring`): the deployment key named by `[wiring] deployment_key_env`, the
//! health gate over the `health` module's state, the notification sink, the backup folder and the
//! tenants configuration named by the environment. The providers that need code outside this
//! crate (the provisioning executor over the application catalogue, the signed management
//! channel to the tenant hosts, the enrolled-agent directory and agent link) are supplied by the
//! host process that embeds the service through `run_with`; this binary has none of them yet, so
//! `tenants`, `moves`, `rollouts` and `deploy` report not ready and `run` names each one in the
//! log. Nothing is faked and no default key exists.

use std::path::PathBuf;
use std::process::ExitCode;

use spot_control_plane::config::Config;

fn config_path(args: &[String]) -> Option<PathBuf> {
    let position = args.iter().position(|arg| arg == "--config")?;
    args.get(position + 1).map(PathBuf::from)
}

fn usage() -> ExitCode {
    eprintln!("usage: spot-control-plane run|migrate|check --config FILE | version");
    ExitCode::from(2)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        return usage();
    };
    if command == "version" {
        println!("spot-control-plane {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let Some(path) = config_path(&args) else {
        return usage();
    };
    let config = match Config::from_file(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("config: {error}");
            return ExitCode::FAILURE;
        }
    };
    match command {
        "check" => {
            let clock = spot_core::SystemClock::new();
            let items = spot_control_plane::check(&config, &clock).await;
            let mut failed = false;
            for item in &items {
                if item.ok {
                    println!("ok    {}", item.name);
                } else {
                    failed = true;
                    println!("FAIL  {}: {}", item.name, item.detail);
                }
            }
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        "migrate" => {
            let result = migrate_command(&config).await;
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("migrate: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        "run" => {
            let shutdown = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            let providers = spot_control_plane::wiring::Providers::default();
            match spot_control_plane::run_with(config, providers, shutdown).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("run: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => usage(),
    }
}

async fn migrate_command(config: &Config) -> Result<(), String> {
    let password = std::env::var(&config.database.password_env)
        .map_err(|_| "the database password variable is not set".to_owned())?;
    let db = &config.database;
    let mut pg = spot_postgres::PgConfig::new(&db.host, db.port, &db.dbname, &db.user, &password);
    pg.tls = if db.tls {
        spot_postgres::Tls::Require
    } else {
        spot_postgres::Tls::Disable
    };
    let backend = spot_postgres::PgBackend::connect(pg)
        .await
        .map_err(|_| "the database is unreachable or unsupported".to_owned())?;
    // The module DDL is collected by assembling the service against the pool, over the same
    // module table and the same wiring as `run`.
    spot_control_plane::wiring::install(
        config,
        spot_control_plane::wiring::Providers::default(),
        &|name| std::env::var(name).ok(),
    )
    .map_err(|error| error.to_string())?;
    let service = spot_control_plane::ControlPlane::build(spot_control_plane::BuildDeps {
        config: std::sync::Arc::new(config.clone()),
        pool: Some(backend.pool()),
        clock: std::sync::Arc::new(spot_core::SystemClock::new()),
        audit: std::sync::Arc::new(spot_control_plane::TracingAuditSink),
        nonces: std::sync::Arc::new(spot_signed_command::MemoryNonceStore::new()),
        vault: std::sync::Arc::new(spot_control_plane::auth::FileKeyVault::new(
            config.host_ca.key.clone(),
        )),
        registrations: spot_control_plane::wiring::registrations(),
    })
    .map_err(|error| error.to_string())?;
    spot_control_plane::migrate(&backend.pool(), &service.migrations)
        .await
        .map_err(|error| error.to_string())
}
