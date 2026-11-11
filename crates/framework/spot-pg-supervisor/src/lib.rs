//! Spotledger PostgreSQL supervisor (WP-PRD-02.T1): runs a bundled PostgreSQL as a
//! supervised child process — initialise once, start, wait until ready, stop gracefully,
//! restart after a crash within a budget, clear a stale `postmaster.pid`.
//!
//! This crate is deliberately independent of the future Windows Service wrapper and of
//! the installer: it only knows the filesystem layout of a bundled PostgreSQL and the
//! child process. It works the same on Windows and Linux; platform-specific code is
//! confined to [`platform`] (signal, shutdown and process-death guarantees).
//!
//! Contents:
//! - [`layout`]: [`PgLayout`](layout::PgLayout) and validation of the bundled layout,
//! - [`settings`]: [`PgSettings`](settings::PgSettings) overrides written to `spot.conf`,
//! - [`config`]: [`SupervisorConfig`](config::SupervisorConfig) (timeouts, probe, budget),
//! - [`error`]: the typed [`SupervisorError`](error::SupervisorError),
//! - [`clock`]: the [`Clock`](clock::Clock) seam used for state timestamps and backoff,
//! - [`supervisor`]: the [`Supervisor`](supervisor::Supervisor) state machine and
//!   [`Handle`](supervisor::Handle),
//! - [`events`]: [`SupervisorEvent`](events::SupervisorEvent) stream for the service
//!   wrapper and diagnostics,
//! - [`pidfile`]: stale `postmaster.pid` detection and cleanup,
//! - [`platform`]: the Unix/Windows shutdown and death-watch implementations,
//! - [`extensions`]: the [`ExtensionProvisioner`](extensions::ExtensionProvisioner) seam
//!   through which the host installs `postgis` on first start (ADR-087 section 4.3).
//!
//! A later service-wrapper task calls it like this (no service code lives here):
//!
//! ```no_run
//! use spot_pg_supervisor::config::SupervisorConfig;
//! use spot_pg_supervisor::layout::{Listen, PgLayout};
//! use spot_pg_supervisor::settings::PgSettings;
//! use spot_pg_supervisor::supervisor::Supervisor;
//! use std::time::Duration;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let layout = PgLayout {
//!     bin_dir: "/opt/spot/pgsql/bin".into(),
//!     data_dir: "/var/lib/spot/pgsql".into(),
//!     log_dir: "/var/log/spot/pgsql".into(),
//!     listen: Listen::Localhost,
//!     port: 5433,
//! };
//! layout.validate().await?;
//! let supervisor = Supervisor::new(layout, PgSettings::default(), SupervisorConfig::default())
//!     // The desktop service wrapper adds the host's `extension.install` adapter here; the
//!     // supervisor never depends on `spot-pg-migrate` itself (see [`extensions`]).
//!     // .with_extension_provisioner(host.extension_provisioner())
//!     ;
//! supervisor.ensure_initialised("site-password-change-me").await?;
//! let handle = supervisor.start().await?;
//! handle.ready().await?;      // PostGIS exists by now, or this failed loudly
//! handle.stop(Duration::from_secs(30)).await?; // fast shutdown, `StoppedForced` if killed
//! # Ok(())
//! # }
//! ```

pub mod clock;
pub mod config;
pub mod error;
pub mod events;
pub mod extensions;
pub mod init;
pub mod layout;
pub mod pidfile;
pub mod platform;
pub mod settings;
pub mod supervisor;

pub use config::{
    RestartBudget, STABLE_WINDOW_DEFAULT, START_TIMEOUT_DEFAULT, STOP_GRACE_DEFAULT,
    SupervisorConfig,
};
pub use error::SupervisorError;
pub use events::SupervisorEvent;
pub use extensions::{
    ExtensionProvisioned, ExtensionProvisioner, POSTGIS, ProvisionFailure, REQUIRED_EXTENSIONS,
};
pub use init::InitState;
pub use layout::{Listen, PgLayout};
pub use settings::PgSettings;
pub use supervisor::{Handle, StoppedHow, Supervisor, SupervisorState};
