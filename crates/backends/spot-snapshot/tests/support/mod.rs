//! Test support: a tiny fake PostgreSQL bundle (initdb/postgres/pg_ctl roles) so the
//! stopped-instance provider's guard and failure paths run on every machine and in
//! every CI runner without a bundled PostgreSQL. The supervisor's tests stage the same
//! idea; this one is this crate's own copy (each crate owns its test support). Each
//! test binary compiles the whole module but uses its own half — `stopped.rs` the
//! fake-bundle helpers, `pgdump.rs` the client-tool discovery — so dead code here is
//! expected per binary.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The supervisor config for tests: short timeouts so failures surface fast.
pub fn supervisor_config() -> spot_pg_supervisor::config::SupervisorConfig {
    spot_pg_supervisor::config::SupervisorConfig {
        start_timeout: Duration::from_secs(10),
        stop_grace: Duration::from_secs(2),
        ..spot_pg_supervisor::config::SupervisorConfig::default()
    }
}

/// Writes the fake binaries into `bin_dir` under the three role names.
///
/// # Panics
/// When the binary was not built (cargo sets `CARGO_BIN_EXE_...`) or staging fails.
pub fn stage_fake_binaries(bin_dir: &Path) {
    let source = match std::env::var("CARGO_BIN_EXE_fake-pg") {
        Ok(path) if !path.is_empty() => path,
        _ => panic!("CARGO_BIN_EXE_fake-pg must be set by cargo test"),
    };
    for role in ["postgres", "initdb", "pg_ctl"] {
        let target = bin_dir.join(role);
        std::fs::copy(&source, &target)
            .unwrap_or_else(|e| panic!("fake-pg could not be staged as {role}: {e}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&target)
                .unwrap_or_else(|e| panic!("staged {role} has no metadata: {e}"))
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&target, permissions)
                .unwrap_or_else(|e| panic!("staged {role} not executable: {e}"));
        }
    }
}

#[path = "../../../../framework/spot-pg-supervisor/tests/common/ports.rs"]
mod ports;

/// A port claimed for this test: nobody else binds it before the fake does.
pub fn free_port() -> u16 {
    ports::claimed_free_port()
}

/// Polls a TCP port until it accepts (bounded), answering "is the instance up?".
pub async fn wait_port(port: u16, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// A small file tree a fake cluster holds (deterministic content for checksums).
pub fn marker_files(data_dir: &Path, branches: usize) {
    for branch in 0..branches {
        let dir = data_dir.join(format!("branch-{branch:03}"));
        std::fs::create_dir_all(&dir).expect("branch dir");
        let mut file = std::fs::File::create(dir.join("rows.bin")).expect("rows file");
        file.write_all(format!("branch {branch} rows\n").as_bytes())
            .expect("rows content");
    }
}

/// The client tools the pg-dump tests need (never server binaries). Compiled for
/// every test binary but referenced only by tests/pgdump.rs (dead_code is expected
/// in the others — the supervisor's `free_port`/`wait_port` users are separate).
const PG_TOOLS: [&str; 2] = ["pg_dump", "pg_restore"];

/// The PostgreSQL client-tools directory for the pg-dump tests: `SPOT_TEST_PG_BIN`
/// (a layout root whose `bin` subdirectory holds the tools) when set, else the first
/// of the standard install layouts that carries both tools, else an empty path (the
/// caller then leaves resolution to `PATH` or skips loudly).
///
/// # Panics
/// Never: a failed discovery is an empty directory the callers handle.
#[allow(dead_code)]
pub fn pg_bin_dir() -> PathBuf {
    if let Ok(root) = std::env::var("SPOT_TEST_PG_BIN")
        && !root.is_empty()
    {
        let dir = PathBuf::from(root).join("bin");
        if has_tools(&dir) {
            return dir;
        }
    }
    for dir in [
        "/usr/lib/postgresql/18/bin",
        "/usr/pgsql-18/bin",
        "/usr/local/bin",
        "/usr/bin",
    ] {
        let dir = PathBuf::from(dir);
        if has_tools(&dir) {
            return dir;
        }
    }
    PathBuf::new()
}

/// Whether a directory holds both client tools; an empty directory defers to `PATH`.
#[allow(dead_code)]
fn has_tools(dir: &Path) -> bool {
    if dir.as_os_str().is_empty() {
        return PG_TOOLS.iter().all(|tool| which_on_path(tool).is_some());
    }
    PG_TOOLS.iter().all(|tool| dir.join(tool).is_file())
}

/// A minimal `PATH` scan (no external crate; the tool names are fixed literals).
#[allow(dead_code)]
fn which_on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(tool);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Panics when no client tools can be located: the gate (`SPOT_TEST_PG_URL` was set)
/// promised a real PostgreSQL, and a harness that cannot run its round trip must be
/// loud, exactly like the URL gate itself.
///
/// # Panics
/// When [`pg_bin_dir`] found nothing anywhere.
#[allow(dead_code)]
pub fn require_pg_bin_dir() {
    let dir = pg_bin_dir();
    if dir.as_os_str().is_empty() {
        panic!(
            "SPOT_TEST_PG_URL is set but no pg_dump/pg_restore were found \
             (set SPOT_TEST_PG_BIN to the PostgreSQL layout root)"
        );
    }
}

/// The credentialed URL of one test database config: `user[:password]@host:port/db`.
/// The password rides the URL in-process only — the providers move it into the child
/// environment, never onto a command line, a log, or a manifest.
#[allow(dead_code)]
pub fn credentialed_url(config: &spot_postgres::PgConfig) -> String {
    let credentials = if config.password.is_empty() {
        config.user.clone()
    } else {
        format!("{}:{}", config.user, config.password)
    };
    format!(
        "postgres://{credentials}@{}:{}/{}",
        config.host, config.port, config.dbname
    )
}
