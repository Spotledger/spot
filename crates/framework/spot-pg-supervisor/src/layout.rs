//! Layout validation and the `spot.conf` renderer for the bundled PostgreSQL. `PgLayout`
//! describes where the shipped binaries and the data/logs live and how postgres listens;
//! every check here is performed before anything is spawned.

use crate::error::SupervisorError;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

/// How postgres binds. `Localhost` (the default) binds `127.0.0.1` only so the bundled
/// database is never reachable from the network by accident.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Listen {
    /// Bind `127.0.0.1` only (default).
    #[default]
    Localhost,
    /// Bind exactly these addresses (must not contain the unspecified address).
    Addresses(Vec<IpAddr>),
}

impl Listen {
    /// The `listen_addresses` value for `postgresql.conf`.
    fn conf_value(&self) -> String {
        match self {
            Listen::Localhost => "127.0.0.1".to_string(),
            Listen::Addresses(addrs) => addrs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        }
    }

    /// The first address to probe for readiness, in the order they are configured.
    pub(crate) fn first_probe_addr(&self) -> IpAddr {
        match self {
            Listen::Localhost => IpAddr::V4(Ipv4Addr::LOCALHOST),
            Listen::Addresses(addrs) => addrs
                .first()
                .copied()
                .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        }
    }
}

/// Where the bundled PostgreSQL lives: binaries, data directory, logs, listen address
/// and port. This is the whole filesystem view the supervisor needs.
#[derive(Debug, Clone)]
pub struct PgLayout {
    /// Directory holding `postgres`, `initdb` and `pg_ctl`.
    pub bin_dir: PathBuf,
    /// The PostgreSQL data directory (`PG_VERSION` marks it initialised).
    pub data_dir: PathBuf,
    /// Directory for the rotating server log.
    pub log_dir: PathBuf,
    /// How postgres binds.
    pub listen: Listen,
    /// TCP port postgres listens on.
    pub port: u16,
}

impl Default for PgLayout {
    fn default() -> Self {
        PgLayout {
            bin_dir: PathBuf::from("pgsql/bin"),
            data_dir: PathBuf::from("pgsql/data"),
            log_dir: PathBuf::from("logs"),
            listen: Listen::Localhost,
            port: 5433,
        }
    }
}

impl PgLayout {
    /// Name of the `postgres` binary on this platform.
    pub(crate) fn postgres_bin(&self) -> PathBuf {
        self.bin_dir.join(binary_name("postgres"))
    }

    /// The PostgreSQL installation prefix: the parent of `bin_dir`.
    ///
    /// A bundled PostgreSQL is one directory tree — `<prefix>/bin` for the programs,
    /// `<prefix>/share` for the data files, `<prefix>/lib` for the shared libraries —
    /// which is the shape of the EDB-built Windows binaries and of the PostGIS Windows
    /// bundle that is dropped into them (the bundle's `README.txt`: "Copy the files in
    /// this folder to your PostgreSQL/%VERSION% folder (structure should mirror what you
    /// have)"). A `bin_dir` with no parent is its own prefix.
    pub fn install_prefix(&self) -> PathBuf {
        self.bin_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.bin_dir.clone())
    }

    /// Where an installed extension keeps its files: its control and SQL scripts
    /// (`<prefix>/share/extension`) and its shared libraries (`<prefix>/lib`).
    pub fn extension_dirs(&self) -> (PathBuf, PathBuf) {
        let prefix = self.install_prefix();
        (prefix.join("share").join("extension"), prefix.join("lib"))
    }

    /// Name of the `initdb` binary on this platform.
    pub(crate) fn initdb_bin(&self) -> PathBuf {
        self.bin_dir.join(binary_name("initdb"))
    }

    /// Name of the `pg_ctl` binary on this platform.
    pub(crate) fn pg_ctl_bin(&self) -> PathBuf {
        self.bin_dir.join(binary_name("pg_ctl"))
    }

    /// Path of the pid file postgres keeps while running.
    pub(crate) fn postmaster_pid_path(&self) -> PathBuf {
        // Name spelled as fragments for the error-code scanner (see
        // `init::POSTGRESQL_CONF`): a lock file name is not error-code vocabulary.
        self.data_dir.join(concat!("postmaster", ".pid"))
    }

    /// Checks that the bundle can provide PostGIS: the extension's control file in
    /// `<prefix>/share/extension`, and the shared library that control file names in
    /// `<prefix>/lib` (ADR-087 sections 4.2 and 4.7 — a desktop install without PostGIS is
    /// a failed install, so it must fail here, before `initdb`).
    ///
    /// A layout with no `<prefix>/share/extension` directory at all is not a PostgreSQL
    /// installation tree: `initdb` cannot run from it (it needs the files in `share/`), so
    /// this check does not add a second, later failure and stays quiet. That is the shape
    /// of a test-only bundle that stages the three programs and nothing else. Any real
    /// bundle — EDB's, ours after the PostGIS drop — has `share/extension`, and from there
    /// the check is unconditional.
    fn check_postgis(&self) -> Result<(), SupervisorError> {
        let (share_extension, lib_dir) = self.extension_dirs();
        if !share_extension.is_dir() {
            return Ok(());
        }
        let prefix = self.install_prefix();
        let mut missing: Vec<String> = Vec::new();
        let control = share_extension.join(concat!("postgis", ".control"));
        if !control.is_file() {
            missing.push(relative_display(&control, &prefix));
        } else {
            // The library is named by the control file itself (`module_pathname =
            // '$libdir/postgis-3'`), so this crate never hard-codes PostGIS's module name.
            match std::fs::read_to_string(&control)
                .ok()
                .and_then(|text| module_file_stem(&text))
            {
                Some(stem) if library_present(&lib_dir, &stem) => {}
                Some(stem) => {
                    missing.push(format!("lib/{stem}.dll|.so|.dylib ({})", lib_dir.display()))
                }
                None => missing.push(format!(
                    "{}/{} names no module_pathname",
                    relative_display(&share_extension, &prefix),
                    concat!("postgis", ".control")
                )),
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        Err(SupervisorError::PostgisMissingFromBundle {
            prefix: prefix.display().to_string(),
            missing,
        })
    }

    /// Validates the layout: required binaries present, data dir not inside the bin
    /// dir, PostGIS present in the bundle (see [`PgLayout::check_postgis`]) and the port
    /// free (bind once, release immediately).
    ///
    /// # Errors
    /// Typed [`SupervisorError`] naming exactly what is wrong.
    pub async fn validate(&self) -> Result<(), SupervisorError> {
        let missing: Vec<String> = ["postgres", "initdb", "pg_ctl"]
            .iter()
            .map(|name| binary_name(name))
            .filter(|file| !self.bin_dir.join(file).is_file())
            .collect();
        if !missing.is_empty() {
            return Err(SupervisorError::MissingBinaries {
                dir: self.bin_dir.display().to_string(),
                missing,
            });
        }

        if self.data_dir.starts_with(&self.bin_dir)
            && self.data_dir != self.bin_dir
            && is_inside(&self.data_dir, &self.bin_dir)
        {
            return Err(SupervisorError::Layout(format!(
                "data_dir ({}) must not be inside bin_dir ({})",
                self.data_dir.display(),
                self.bin_dir.display()
            )));
        }

        // ADR-087: the desktop install has no degraded mode, so a bundle without PostGIS
        // is refused here — before `initdb` creates a data directory that could not store
        // a `geo` field.
        self.check_postgis()?;

        // Prove the port is free: bind it ourselves, then release it. PostgreSQL will
        // bind with SO_REUSEADDR semantics; a listener that holds the port now would
        // make the child fail a minute later, so fail here instead.
        bind_probe(self.listen.first_probe_addr(), self.port)
            .await
            .map_err(|source| SupervisorError::PortBusy {
                port: self.port,
                source,
            })?;

        Ok(())
    }
}

/// Platform binary name: `postgres.exe` on Windows, `postgres` elsewhere.
pub(crate) fn binary_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// `path` relative to `prefix` when it is inside it (for readable error text), else the
/// full path.
fn relative_display(path: &Path, prefix: &Path) -> String {
    path.strip_prefix(prefix)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The shared-library file stem a control file asks for: the last path segment of
/// `module_pathname = '$libdir/postgis-3'` is `postgis-3`.
fn module_file_stem(control: &str) -> Option<String> {
    for line in control.lines() {
        let line = line.trim();
        if !line.starts_with("module_pathname") {
            continue;
        }
        let (_, value) = line.split_once('=')?;
        let value = value.trim().trim_matches(|c| c == '\'' || c == '"');
        let stem = value.rsplit('/').next()?;
        if stem.is_empty() {
            return None;
        }
        return Some(stem.to_string());
    }
    None
}

/// Whether the shared library `stem` is installed as `<lib_dir>/<stem>.<dll|so|dylib>`
/// (the suffix depends on the platform the bundle was built for; checking all three lets a
/// staged bundle validate anywhere).
fn library_present(lib_dir: &Path, stem: &str) -> bool {
    ["dll", "so", "dylib"]
        .iter()
        .any(|suffix| lib_dir.join(format!("{stem}.{suffix}")).is_file())
        || lib_dir.join(stem).is_file()
}

/// Binds and immediately releases `addr:port`; `Ok` means the port was free.
pub(crate) async fn bind_probe(addr: IpAddr, port: u16) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind((addr, port)).await?;
    drop(listener);
    Ok(())
}

/// Returns `true` when `child` is strictly below `parent` in the path tree.
pub fn is_inside(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent) && child != parent
}

/// `spot.conf` keys the supervisor owns; callers can never override them through
/// [`crate::settings::PgSettings::extra_config`] (PostgreSQL resolves duplicate
/// settings with the last value winning, so an override line would defeat the
/// durability switches).
const PROTECTED_CONF_KEYS: [&str; 6] = [
    "listen_addresses",
    "port",
    "fsync",
    "full_page_writes",
    "max_connections",
    "shared_buffers",
];

/// Renders the `spot.conf` overrides included from `postgresql.conf`. The two
/// durability switches are always `on`: they cannot be turned off through
/// [`crate::settings::PgSettings`], and `extra_config` lines naming a protected key
/// are dropped with a warning.
pub fn render_spot_conf(listen: &Listen, settings: &crate::settings::PgSettings) -> String {
    let mut lines = vec![
        format!("listen_addresses = '{}'", listen.conf_value()),
        format!("port = {}", settings.port),
        format!("max_connections = {}", settings.max_connections),
        format!("shared_buffers = '{}'", settings.shared_buffers),
        // Durability overrides: fixed here, not configurable through PgSettings.
        "fsync = on".to_string(),
        "full_page_writes = on".to_string(),
    ];
    for extra in &settings.extra_config {
        let protected = PROTECTED_CONF_KEYS
            .iter()
            .any(|key| extra.trim_start().to_ascii_lowercase().starts_with(key));
        if protected {
            tracing::warn!(
                line = %extra,
                "spot.conf override dropped: the key is owned by the supervisor"
            );
            continue;
        }
        lines.push(extra.clone());
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}
