//! A disposable PostgreSQL cluster the harness starts, kills and restarts itself.
//!
//! [`ScratchCluster`] runs its **own** cluster in a temporary directory on a free port through
//! `spot-pg-supervisor` ([`Supervisor`]); it never touches the shared server behind
//! `SPOT_TEST_PG_URL`, because it kills the server. The settings make crash tests meaningful:
//! `fsync = on` and `full_page_writes = on` (forced by the supervisor), `synchronous_commit = on`,
//! a small `shared_buffers` and `max_connections = 300`.
//!
//! It refuses to run as root (initdb refuses too), and the kill path is Linux-only: elsewhere the
//! crash methods compile away and the caller prints `SKIP`, because the Windows crash work belongs
//! to WP-PRD-05. The cluster is stopped and its directory removed on drop, including when a test
//! panics.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use spot_pg_supervisor::config::SupervisorConfig;
use spot_pg_supervisor::layout::{Listen, PgLayout};
use spot_pg_supervisor::settings::PgSettings;
use spot_pg_supervisor::supervisor::{Handle, Supervisor};
use spot_postgres::PgConfig;

/// The bundled-PostgreSQL binaries the CI host has, when `SPOT_PG_BIN` is unset.
pub const DEFAULT_PG_BIN: &str = "/usr/lib/postgresql/18/bin";

/// Where the PostgreSQL binaries are, or `None` when they are absent.
///
/// `SPOT_PG_BIN` wins; otherwise [`DEFAULT_PG_BIN`] is used when its `initdb` exists.
pub fn pg_bin_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("SPOT_PG_BIN")
        && !dir.is_empty()
    {
        let path = PathBuf::from(dir);
        return path.join("initdb").is_file().then_some(path);
    }
    let path = PathBuf::from(DEFAULT_PG_BIN);
    path.join("initdb").is_file().then_some(path)
}

/// Whether the process runs as root (`id -u` is 0).
pub fn is_root() -> bool {
    match std::process::Command::new("id").arg("-u").output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim() == "0",
        Err(_) => false,
    }
}

/// How to build the scratch cluster.
#[derive(Clone, Debug)]
pub struct ClusterConfig {
    /// The directory holding `postgres`, `initdb` and `pg_ctl`.
    pub bin_dir: PathBuf,
    /// The superuser password (kept off the command line by the supervisor).
    pub password: String,
    /// The database the backend connects to (created with the ICU collation provider).
    pub database: String,
    /// `max_connections`.
    pub max_connections: u32,
    /// `shared_buffers`.
    pub shared_buffers: String,
    /// Extra `spot.conf` lines that are not supervisor-protected.
    pub extra_config: Vec<String>,
    /// Start timeout.
    pub start_timeout: Duration,
    /// Stop grace.
    pub stop_grace: Duration,
    /// Restarts allowed inside the budget window.
    pub restart_budget: u32,
    /// Append `fsync = off` / `synchronous_commit = off` after init. Only the `#[ignore]`d
    /// honesty demonstration uses this; it is the one way to make acknowledged commits losable.
    pub unsafe_durability: bool,
}

impl ClusterConfig {
    /// The documented settings for a crash-test cluster.
    pub fn new(bin_dir: PathBuf) -> ClusterConfig {
        ClusterConfig {
            bin_dir,
            password: "spot-l8-faults".to_owned(),
            database: "spot_faults".to_owned(),
            max_connections: 300,
            shared_buffers: "32MB".to_owned(),
            extra_config: vec!["synchronous_commit = on".to_owned()],
            start_timeout: Duration::from_secs(60),
            stop_grace: Duration::from_secs(15),
            restart_budget: 10,
            unsafe_durability: false,
        }
    }
}

/// Everything that can go wrong starting or controlling a scratch cluster.
#[derive(Debug)]
pub enum ClusterError {
    /// No PostgreSQL binaries were found.
    NoBinaries,
    /// The harness refuses to run as root.
    RunningAsRoot,
    /// The supervisor failed.
    Supervisor(spot_pg_supervisor::SupervisorError),
    /// A path or directory operation failed.
    Io(String),
    /// A `psql` invocation failed.
    Psql(String),
    /// `initdb` failed.
    Initdb(String),
    /// The postmaster pid could not be read.
    NoPid,
    /// The kill path is not available on this platform.
    Unsupported,
    /// The server did not recover inside the deadline.
    RecoveryTimeout(Duration),
}

impl std::fmt::Display for ClusterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClusterError::NoBinaries => {
                f.write_str("no PostgreSQL binaries: set SPOT_PG_BIN or install PostgreSQL 18")
            }
            ClusterError::RunningAsRoot => {
                f.write_str("refusing to run a scratch PostgreSQL as root")
            }
            ClusterError::Supervisor(e) => write!(f, "supervisor: {e}"),
            ClusterError::Io(what) => write!(f, "io: {what}"),
            ClusterError::Psql(what) => write!(f, "psql: {what}"),
            ClusterError::Initdb(what) => write!(f, "initdb: {what}"),
            ClusterError::NoPid => f.write_str("no postmaster.pid: the cluster is not running"),
            ClusterError::Unsupported => {
                f.write_str("killing a cluster is Linux-only; the Windows work is WP-PRD-05")
            }
            ClusterError::RecoveryTimeout(d) => {
                write!(f, "the server did not recover within {d:?}")
            }
        }
    }
}

impl std::error::Error for ClusterError {}

impl From<spot_pg_supervisor::SupervisorError> for ClusterError {
    fn from(e: spot_pg_supervisor::SupervisorError) -> Self {
        ClusterError::Supervisor(e)
    }
}

/// A disposable PostgreSQL cluster owned by one test.
pub struct ScratchCluster {
    config: ClusterConfig,
    supervisor: Supervisor,
    handle: Option<Handle>,
    temp: PathBuf,
    layout: PgLayout,
    port: u16,
}

impl ScratchCluster {
    /// Starts a fresh cluster in a temporary directory on a free port and creates the database.
    pub async fn start(config: ClusterConfig) -> Result<ScratchCluster, ClusterError> {
        if is_root() {
            return Err(ClusterError::RunningAsRoot);
        }
        let temp = unique_temp_dir()?;
        let data_dir = temp.join("data");
        let log_dir = temp.join("logs");
        std::fs::create_dir_all(&data_dir).map_err(io_err)?;
        std::fs::create_dir_all(&log_dir).map_err(io_err)?;
        let port = free_port()?;
        let layout = PgLayout {
            bin_dir: config.bin_dir.clone(),
            data_dir,
            log_dir,
            listen: Listen::Localhost,
            port,
        };
        // Debian's PostgreSQL defaults `unix_socket_directories` to /var/run/postgresql, which the
        // CI host does not have and cannot create without root; point it at our temp directory.
        let mut extra_config = config.extra_config.clone();
        extra_config.push(format!("unix_socket_directories = '{}'", temp.display()));
        let settings = PgSettings {
            port,
            max_connections: config.max_connections,
            shared_buffers: config.shared_buffers.clone(),
            extra_config,
        };
        let supervisor_config = SupervisorConfig {
            start_timeout: config.start_timeout,
            stop_grace: config.stop_grace,
            restart_budget: spot_pg_supervisor::RestartBudget {
                max_restarts: config.restart_budget,
                window: Duration::from_secs(3600),
            },
            ..SupervisorConfig::default()
        };
        let supervisor = Supervisor::new(layout.clone(), settings, supervisor_config);
        run_initdb(&layout, &config.password)?;
        supervisor.ensure_initialised(&config.password).await?;
        ensure_spot_conf_included(&layout.data_dir)?;
        if config.unsafe_durability {
            append_spot_conf(&layout.data_dir, "fsync = off\nsynchronous_commit = off\n")?;
        }
        let handle = supervisor.start().await?;
        handle.ready().await?;
        let cluster = ScratchCluster {
            config,
            supervisor,
            handle: Some(handle),
            temp,
            layout,
            port,
        };
        // The supervisor calls the server ready once its port accepts a TCP connection, which a
        // postmaster does while it is still replaying WAL ("the database system is starting up"
        // on the first query). Wait for a real query before the first statement.
        cluster
            .wait_until_accepting(cluster.config.start_timeout)
            .await?;
        cluster.create_database()?;
        Ok(cluster)
    }

    /// Polls `SELECT 1` until the server answers or `timeout` passes; the last `psql` error is
    /// in the failure.
    async fn wait_until_accepting(&self, timeout: Duration) -> Result<(), ClusterError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.psql_on("postgres", "SELECT 1") {
                Ok(_) => return Ok(()),
                Err(error) if std::time::Instant::now() >= deadline => return Err(error),
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    fn create_database(&self) -> Result<(), ClusterError> {
        self.create_database_named(&self.config.database)
    }

    /// Creates another database on the cluster with the ICU collation provider.
    pub fn create_database_named(&self, name: &str) -> Result<(), ClusterError> {
        let sql = format!(
            "CREATE DATABASE \"{name}\" TEMPLATE template0 ENCODING 'UTF8' \
             LOCALE 'C' LOCALE_PROVIDER icu ICU_LOCALE 'und'"
        );
        self.psql_on("postgres", &sql)?;
        Ok(())
    }

    /// The port PostgreSQL listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The database the backend should connect to.
    pub fn database(&self) -> &str {
        &self.config.database
    }

    /// The data directory.
    pub fn data_dir(&self) -> &Path {
        &self.layout.data_dir
    }

    /// The directory holding the rotating server logs.
    pub fn log_dir(&self) -> &Path {
        &self.layout.log_dir
    }

    /// The server's standard-output log file, if it exists.
    pub fn server_log_path(&self) -> PathBuf {
        self.layout.log_dir.join("server.log")
    }

    /// Lines in the server log that name a durability fault: a bad page, a checksum failure, a
    /// corrupt file or a `PANIC`. Empty means recovery was clean. Never carries a secret (the
    /// server's own log does not hold the password).
    pub fn recovery_errors(&self) -> Vec<String> {
        let mut out = Vec::new();
        for name in ["server.log", "server.err.log"] {
            let Ok(text) = std::fs::read_to_string(self.layout.log_dir.join(name)) else {
                continue;
            };
            for line in text.lines() {
                let l = line.to_ascii_lowercase();
                if l.contains("invalid page")
                    || l.contains("checksum")
                    || l.contains("corrupt")
                    || l.contains("panic")
                {
                    let line = line.trim();
                    out.push(if line.len() > 200 {
                        line[..200].to_owned()
                    } else {
                        line.to_owned()
                    });
                }
            }
        }
        out
    }

    /// The current postmaster pid, read from `postmaster.pid`.
    pub fn pid(&self) -> Option<u32> {
        let text = std::fs::read_to_string(self.layout.data_dir.join("postmaster.pid")).ok()?;
        text.lines().next()?.trim().parse::<u32>().ok()
    }

    /// A [`PgConfig`] for a backend on this cluster.
    pub fn backend_config(&self) -> PgConfig {
        self.backend_config_named(&self.config.database)
    }

    /// A [`PgConfig`] for a backend on the named database of this cluster.
    pub fn backend_config_named(&self, dbname: &str) -> PgConfig {
        let mut c = PgConfig::new(
            "127.0.0.1",
            self.port,
            dbname,
            "spot",
            &self.config.password,
        );
        c.application_name = "spot-l8-faults".to_owned();
        c.pool_max_size = 32;
        c.connect_timeout = Duration::from_secs(5);
        c.pool_wait_timeout = Duration::from_secs(30);
        c
    }

    /// Runs `sql` through `psql` on the given database and returns trimmed stdout.
    pub fn psql_on(&self, database: &str, sql: &str) -> Result<String, ClusterError> {
        let out = std::process::Command::new(self.layout.bin_dir.join("psql"))
            .arg("-h")
            .arg("127.0.0.1")
            .arg("-p")
            .arg(self.port.to_string())
            .arg("-U")
            .arg("spot")
            .arg("-d")
            .arg(database)
            .arg("-v")
            .arg("ON_ERROR_STOP=1")
            .arg("-Atc")
            .arg(sql)
            .env("PGPASSWORD", &self.config.password)
            .output()
            .map_err(|e| ClusterError::Psql(e.to_string()))?;
        if !out.status.success() {
            return Err(ClusterError::Psql(
                String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Runs `sql` through `psql` on the cluster's database.
    pub fn psql(&self, sql: &str) -> Result<String, ClusterError> {
        self.psql_on(&self.config.database, sql)
    }

    /// Whether the server answers `SELECT 1`.
    pub fn is_up(&self) -> bool {
        self.psql_on("postgres", "SELECT 1").is_ok()
    }

    /// Asks for a fast shutdown and waits for the child to exit.
    pub async fn stop_graceful(&mut self) -> Result<(), ClusterError> {
        if let Some(handle) = self.handle.take() {
            handle.stop(self.config.stop_grace).await?;
        }
        Ok(())
    }

    /// `SIGKILL`s the postmaster **and its children** (a crash, no checkpoint).
    #[cfg(unix)]
    pub fn kill9(&self) -> Result<(), ClusterError> {
        let pid = self.pid().ok_or(ClusterError::NoPid)?;
        let mut pids = descendants(pid);
        pids.push(pid);
        // Children first, so none survives its parent; then the postmaster.
        for p in pids.iter().rev() {
            kill9_pid(*p);
        }
        wait_gone(pid, Duration::from_secs(15));
        Ok(())
    }

    /// The kill path is Linux-only.
    #[cfg(not(unix))]
    pub fn kill9(&self) -> Result<(), ClusterError> {
        Err(ClusterError::Unsupported)
    }

    /// Waits for crash recovery to finish after a [`ScratchCluster::kill9`] and returns how long
    /// it took.
    ///
    /// The supervisor restarts the postmaster on its own; this waits until a real `SELECT 1`
    /// succeeds (PostgreSQL does not accept connections until crash recovery is done), which is
    /// the honest definition of "recovered".
    pub async fn restart_after_kill(&self) -> Result<Duration, ClusterError> {
        let deadline = Duration::from_secs(60);
        let started = Instant::now();
        loop {
            if self.is_up() {
                return Ok(started.elapsed());
            }
            if started.elapsed() >= deadline {
                return Err(ClusterError::RecoveryTimeout(deadline));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Waits until the supervisor is ready again, for callers that want the state transition
    /// rather than a query. Never blocks forever: a failed state is an error.
    pub async fn wait_ready(&self) -> Result<(), ClusterError> {
        match &self.handle {
            Some(handle) => handle.ready().await.map_err(ClusterError::Supervisor),
            None => Err(ClusterError::Unsupported),
        }
    }

    /// The supervisor, for tests that want to inspect its state.
    pub fn supervisor(&self) -> &Supervisor {
        &self.supervisor
    }

    /// Kills the postmaster and removes the temporary directory (best effort, synchronous).
    fn cleanup(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pid) = self.pid() {
                let mut pids = descendants(pid);
                pids.push(pid);
                for p in pids.iter().rev() {
                    kill9_pid(*p);
                }
                wait_gone(pid, Duration::from_secs(5));
            }
        }
        let _ = std::fs::remove_dir_all(&self.temp);
    }
}

impl Drop for ScratchCluster {
    fn drop(&mut self) {
        // Try a graceful stop on a short-lived thread with its own runtime; the monitor task runs
        // on the caller's runtime, which is still alive while locals are dropped.
        if let Some(handle) = self.handle.take() {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    let _ = rt.block_on(handle.stop(Duration::from_secs(8)));
                }
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(Duration::from_secs(12));
        }
        self.cleanup();
    }
}

fn io_err(e: std::io::Error) -> ClusterError {
    ClusterError::Io(e.to_string())
}

/// A fresh directory under the system temp dir, unique to this process and call.
fn unique_temp_dir() -> Result<PathBuf, ClusterError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("spot-l8-faults-{pid}-{n}-{nanos}"));
    std::fs::create_dir_all(&dir).map_err(io_err)?;
    Ok(dir)
}

/// A free TCP port on the loopback interface.
fn free_port() -> Result<u16, ClusterError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(io_err)?;
    let port = listener.local_addr().map_err(io_err)?.port();
    drop(listener);
    Ok(port)
}

/// Appends `lines` to the cluster's `spot.conf` (used only for the unsafe-durability demo).
fn append_spot_conf(data_dir: &Path, lines: &str) -> Result<(), ClusterError> {
    use std::io::Write;
    let path = data_dir.join("spot.conf");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(io_err)?;
    file.write_all(lines.as_bytes()).map_err(io_err)?;
    Ok(())
}

/// Runs `initdb` for the cluster.
///
/// The supervisor's `ensure_initialised` writes its `--pwfile` **inside** the data directory and
/// then asks `initdb` to initialise that same directory, which `initdb` refuses because it is not
/// empty ("exists but is not empty"). Until that is fixed in `spot-pg-supervisor`, the harness
/// initialises the cluster itself, with the password file beside the data directory, and then
/// hands the initialised cluster to the supervisor (whose `ensure_initialised` then only writes
/// `spot.conf`). This is recorded under `## Open questions` in the task report.
fn run_initdb(layout: &PgLayout, password: &str) -> Result<(), ClusterError> {
    let parent = layout
        .data_dir
        .parent()
        .ok_or_else(|| ClusterError::Io("the data directory has no parent".to_owned()))?;
    let pw_path = parent.join("initdb.pw");
    std::fs::write(&pw_path, password).map_err(io_err)?;
    let out = std::process::Command::new(layout.bin_dir.join("initdb"))
        .arg("-D")
        .arg(&layout.data_dir)
        .arg("-U")
        .arg("spot")
        .arg("--auth-local=peer")
        .arg("--auth=scram-sha-256")
        .arg("--encoding=UTF8")
        .arg("--no-instructions")
        .arg(format!("--pwfile={}", pw_path.display()))
        .output()
        .map_err(io_err)?;
    let _ = std::fs::remove_file(&pw_path);
    if !out.status.success() {
        return Err(ClusterError::Initdb(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    Ok(())
}

/// Ensures `postgresql.conf` includes `spot.conf` (the supervisor does this only on the fresh-init
/// path, which the workaround above skips).
fn ensure_spot_conf_included(data_dir: &Path) -> Result<(), ClusterError> {
    use std::io::Write;
    let path = data_dir.join("postgresql.conf");
    let mut text = std::fs::read_to_string(&path).map_err(io_err)?;
    if text.lines().any(|l| l.trim() == "include = 'spot.conf'") {
        return Ok(());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("# added by spot-l8-faults: bundled overrides\n");
    text.push_str("include = 'spot.conf'\n");
    let mut file = std::fs::File::create(&path).map_err(io_err)?;
    file.write_all(text.as_bytes()).map_err(io_err)?;
    Ok(())
}

/// `kill -9 <pid>`, best effort (a pid that already died is not an error).
fn kill9_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Waits until `/proc/<pid>` is gone, or the timeout elapses.
fn wait_gone(pid: u32, timeout: Duration) {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The descendant pids of `pid`, read from `/proc/*/stat` (Linux only; empty elsewhere).
#[cfg(unix)]
fn descendants(pid: u32) -> Vec<u32> {
    let mut children: Vec<(u32, u32)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(child) = name.parse::<u32>() else {
            continue;
        };
        if let Some(parent) = ppid_of(child) {
            children.push((child, parent));
        }
    }
    let mut out = Vec::new();
    let mut frontier = vec![pid];
    while let Some(parent) = frontier.pop() {
        for (child, p) in &children {
            if *p == parent && !out.contains(child) {
                out.push(*child);
                frontier.push(*child);
            }
        }
    }
    out
}

/// The parent pid of `pid` from `/proc/<pid>/stat`, or `None`.
#[cfg(unix)]
fn ppid_of(pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesised and may contain spaces or parens: parse after the last
    // ')' so the fields line up: state, ppid, ...
    let rest = text.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse::<u32>().ok()
}
