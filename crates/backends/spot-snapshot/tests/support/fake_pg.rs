//! The fake PostgreSQL bundle: behaves like initdb/postgres/pg_ctl driven by its file
//! name and a per-data-dir `.env` file (edition-2024-safe). The supervisor's crates
//! use the same idea; this one supports the snapshot tests' needs:
//!
//! - `initdb`: writes `PG_VERSION` (18), `spot.conf`, a few marker files; records the
//!   argv (tests prove no password travelled as an argument). `FAKE_INITDB_FAIL=1`
//!   fails after writing files (half-initialised).
//! - `postgres`: binds after `FAKE_LISTEN_DELAY_MS`; on SIGTERM exits 0;
//!   `FAKE_CRASH_ON_STOP=1` exits 1 (exercises the forced path harmlessly).
//!   The port is the first of `PGPORT` (the supervisor's spawn env — exactly how the
//!   real postmaster lets the env override the config file), then `FAKE_PORT`.
//! - `pg_ctl`: accepts `stop` (exits 0) — the supervisor's fallback for a refused
//!   signal stop; never used to bypass the supervisor in this crate.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let role = detect_role(&args);
    match role.as_str() {
        "initdb" => run_initdb(&args),
        "postgres" => run_postgres(&args),
        "pg_ctl" => run_pg_ctl(&args),
        other => {
            eprintln!("fake-pg: unknown role {other:?}");
            std::process::exit(2);
        }
    }
}

/// Role from argv[0]'s file name (minus `.exe`).
fn detect_role(args: &[String]) -> String {
    args.first()
        .map(|path| {
            Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .unwrap_or_default()
        .trim_end_matches(".exe")
        .to_string()
}

/// Finds the value after `-D` in the arguments.
fn data_dir_from(args: &[String]) -> Option<PathBuf> {
    args.iter()
        .position(|arg| arg == "-D")
        .and_then(|index| args.get(index + 1))
        .map(PathBuf::from)
}

/// Reads the per-data-dir `.env` into a map (child env merged; file wins).
fn read_env_file(path: &Path) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                map.insert(key.trim().to_owned(), value.trim().to_owned());
            }
        }
    }
    map
}

/// initdb: markers in the `-D` dir; `FAKE_INITDB_FAIL=1` fails after writing files.
fn run_initdb(args: &[String]) {
    let data_dir = data_dir_from(args).unwrap_or_else(std::env::temp_dir);
    // Record the argv beside the data dir: tests prove the password travelled only
    // through `--pwfile` (its own path, no value) or the environment.
    let _ = std::fs::write(data_dir.join("fake-initdb-argv.txt"), args.join("\u{1}"));
    if std::env::var("FAKE_INITDB_FAIL").is_ok() {
        let _ = std::fs::create_dir_all(data_dir.join("base"));
        let _ = std::fs::write(data_dir.join("base").join("template.db"), "half\n");
        eprintln!("initdb: fake failure writing the template catalogue");
        std::process::exit(1);
    }
    let _ = std::fs::create_dir_all(&data_dir);
    let markers = [
        ("PG_VERSION", "18\n"),
        ("spot.conf", "port = 5433\n"),
        ("postgresql.conf", "# fake\n"),
    ];
    for (name, contents) in markers {
        if std::fs::write(data_dir.join(name), contents).is_err() {
            eprintln!("fake-pg: could not write {name}");
            std::process::exit(1);
        }
    }
    println!("success (fake)");
    let _ = std::io::stdout().flush();
}

/// postgres: records argv, binds `FAKE_PORT` after the delay, stays until SIGTERM.
fn run_postgres(args: &[String]) {
    let data_dir = data_dir_from(args).unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::write(data_dir.join("fake-postgres-argv.txt"), args.join("\u{1}"));
    let values = read_env_file(&data_dir.join(".env"));
    let setting = |name: &str| {
        values
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
    };
    let port = match std::env::var("PGPORT") {
        // The supervisor exports PGPORT for the postmaster; the real postgres lets the
        // env override `postgresql.conf`'s port, so the fake must too — a restored data
        // dir's old FAKE_PORT must not shadow the supervisor's spawn env.
        Ok(text) => text.parse::<u16>().unwrap_or(5433),
        Err(_) => setting("FAKE_PORT")
            .and_then(|p| p.parse().ok())
            .unwrap_or(5433),
    };
    let delay: u64 = setting("FAKE_LISTEN_DELAY_MS")
        .and_then(|d| d.parse().ok())
        .unwrap_or(0);
    let _ = std::fs::write(
        data_dir.join(concat!("postmaster", ".pid")),
        std::process::id().to_string(),
    );
    if delay > 0 {
        std::thread::sleep(std::time::Duration::from_millis(delay));
    }
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("fake-pg: cannot bind {port}: {error}");
            std::process::exit(1);
        }
    };
    println!("ready (fake) on {port}");
    let _ = std::io::stdout().flush();
    // Real postgres answers the readiness TCP probe by accepting and closing; the
    // supervisor signals fast shutdown with SIGINT, so the fake must leave when it
    // arrives. Unix default action on SIGINT is exit — but a blocking `incoming()`
    // loop may never observe the signal, so the fake polls non-blockingly and lets
    // the default disposition end it (the supervisor's kill path covers the rest).
    let _ = listener.set_nonblocking(true);
    loop {
        match listener.accept() {
            Ok(_) => {} // probe connections are answered by the accept itself
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                eprintln!("fake-pg: accept failed: {error}");
                std::process::exit(1);
            }
        }
        if setting("FAKE_CRASH_ON_STOP").is_some() {
            // Simulate a crash after the first accept: exit non-zero.
            std::process::exit(42);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// pg_ctl: answers `stop` with success (the supervisor's Windows-style fallback).
fn run_pg_ctl(args: &[String]) {
    if args.iter().any(|arg| arg == "stop") {
        println!("pg_ctl: fake stop");
        let _ = std::io::stdout().flush();
    } else {
        eprintln!("fake-pg: pg_ctl only implements stop");
        std::process::exit(1);
    }
}
