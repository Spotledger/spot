//! `run`'s shared body (brief: the SCM entry point): build the real PostgreSQL and app
//! controls from a loaded [`crate::config::ServiceConfig`], drive
//! [`crate::orchestrator::Orchestrator`] through its start order, call `on_ready` the
//! first time it reaches [`crate::orchestrator::Readiness::Ready`] (the Windows dispatcher
//! reports `SERVICE_RUNNING` there; a non-Windows foreground run just logs it), then
//! block until `should_stop` says to stop, and run the stop order.
//!
//! This function is platform-generic (no `cfg(windows)`): a non-Windows host can run it
//! directly from `spot-winsvc run --config PATH` for local development, exactly as the
//! Windows dispatcher runs it from inside `service_main`.

use crate::app::{AppSpec, RealApp};
use crate::config::ServiceConfig;
use crate::error::WinsvcError;
use crate::orchestrator::{Orchestrator, Readiness};
use crate::pg::RealPg;
use spot_pg_supervisor::{PgLayout, Supervisor, SupervisorConfig};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// How long `wait_ready` may take at start-up before giving up (brief default budget).
const APP_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// The margin added beyond the host's own drain budget before PostgreSQL is stopped
/// (brief: "wait for the shutdown grace of the host (`shutdown_grace_ms` + margin)").
const STOP_MARGIN: Duration = Duration::from_secs(5);

/// How often the foreground loop checks `should_stop` once the service is running.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Runs the full orchestrated lifecycle until `should_stop` reports true.
///
/// # Errors
/// Whatever [`Orchestrator::start`] returns (a failed PostgreSQL init/start, or a failed
/// `spot serve` start/readiness).
pub async fn run_foreground(
    config_path: &Path,
    on_ready: impl Fn(),
    should_stop: impl Fn() -> bool,
) -> Result<(), WinsvcError> {
    let config = ServiceConfig::load(config_path)
        .map_err(|e| WinsvcError::failed(format!("loading service config: {e}")))?;

    let pg_data_dir = config.postgres.data_dir.clone();
    let secrets_dir = pg_data_dir.parent().unwrap_or(&pg_data_dir).join("secrets");
    let password = crate::secrets::ensure_secret(
        &secrets_dir.join(crate::secrets::PG_SUPERUSER_PASSWORD_FILE),
    )?;
    // The site keys (brief "Directories": "the PostgreSQL superuser password and the
    // site keys are generated at first start with random values and stored in files
    // readable only by the service account"). Generated the same way, right alongside
    // the password, and never logged or read back into this function beyond existing.
    let _site_keys =
        crate::secrets::ensure_secret(&secrets_dir.join(crate::secrets::SITE_KEYS_FILE))?;

    let layout = PgLayout {
        bin_dir: config.postgres.bin_dir.clone(),
        data_dir: config.postgres.data_dir.clone(),
        log_dir: config.postgres.log_dir.clone(),
        listen: spot_pg_supervisor::Listen::Localhost,
        port: config.postgres.port,
    };
    let supervisor = Arc::new(Supervisor::new(
        layout,
        spot_pg_supervisor::PgSettings::default(),
        SupervisorConfig::default(),
    ));
    supervisor.ensure_initialised(&password).await?;
    let pg = Arc::new(RealPg::new(supervisor));

    let app = Arc::new(RealApp::new(AppSpec {
        binary: config.app.spot_binary.clone(),
        site_root: config.app.site_root.clone(),
        health_addr: config.app.health_addr.clone(),
    }));

    let orchestrator = Orchestrator::new(pg, app);
    orchestrator.start(APP_READY_TIMEOUT).await?;
    if orchestrator.readiness() == Readiness::Ready {
        on_ready();
    }

    while !should_stop() {
        tokio::time::sleep(STOP_POLL_INTERVAL).await;
    }

    let app_grace = config.app.shutdown_grace(STOP_MARGIN);
    orchestrator
        .stop(app_grace, spot_pg_supervisor::STOP_GRACE_DEFAULT)
        .await
}
