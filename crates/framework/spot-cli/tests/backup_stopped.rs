//! WP-PRD-04.T8: `spot backup run --kind stopped`. Real PostgreSQL through `SPOT_TEST_PG_URL` for
//! the site, and a bundled instance (`SPOT_TEST_PG_BIN` names a layout root whose `bin` holds the
//! binaries and whose `data` is a stopped, initialised cluster) for the end-to-end test. A test
//! that needs one of them prints one `SKIPPED` line and passes when it is missing; nothing is
//! faked. The refusal tests lay down marker files only (`PG_VERSION`, `bin/postgres`) because
//! they stop before anything starts.
#![cfg(unix)]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::Env;
use spot_app_api::prelude::RecordId;

/// The port the bundled layout of `PgLayout::default()` uses.
const BUNDLED_PORT: u16 = 5433;

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

fn temp(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spot_backup_t8_{label}_{}", RecordId::new_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn port_in_use() -> bool {
    TcpListener::bind(("127.0.0.1", BUNDLED_PORT)).is_err()
}

struct Rig {
    env: Env,
    target: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.target);
    }
}

impl Rig {
    /// A site with a folder target `main` at `target` (default: a fresh directory).
    async fn new(target: Option<PathBuf>) -> Option<Rig> {
        let env = Env::new("").await?;
        let target = target.unwrap_or_else(|| temp("target"));
        std::fs::create_dir_all(&target).unwrap();
        env.write_common(&["'sample'"], &config_for(&target));
        env.site();
        Some(Rig { env, target })
    }

    fn root(&self) -> &Path {
        &self.env.root.0
    }

    /// Points `admin_url` at the loopback bundled port, the way a desktop install does.
    fn use_bundled_server(&self) {
        let file = self.root().join("common.toml");
        let text = std::fs::read_to_string(&file).unwrap();
        let replaced: String = text
            .lines()
            .map(|line| {
                if line.starts_with("admin_url") {
                    format!("admin_url = \"postgres://postgres:t8-secret-pw@127.0.0.1:{BUNDLED_PORT}/postgres\"")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(file, replaced).unwrap();
    }

    /// Marker files of a bundled install under the root: enough for the layout check, not a
    /// server.
    fn marker_install(&self) -> PathBuf {
        let base = self.root().join("pgsql");
        std::fs::create_dir_all(base.join("data")).unwrap();
        std::fs::create_dir_all(base.join("bin")).unwrap();
        std::fs::write(base.join("data").join("PG_VERSION"), "18\n").unwrap();
        std::fs::write(base.join("bin").join("postgres"), "").unwrap();
        base
    }

    fn spot(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_spot"))
            .arg("--root")
            .arg(self.root())
            .args(args)
            .env("XDG_STATE_HOME", self.root().join("state"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn backup(&self, verb: &str, extra: &[&str]) -> Output {
        let mut args = vec!["backup", verb, "--site", &self.env.site];
        args.extend_from_slice(extra);
        self.spot(&args)
    }

    fn packages(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.target)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "spotbak"))
            .collect()
    }

    /// Nothing at all was written to the target: no package, no scratch directory.
    fn target_untouched(&self) -> bool {
        std::fs::read_dir(&self.target).unwrap().next().is_none()
    }
}

fn config_for(path: &Path) -> String {
    format!(
        "[backup]\ndefault_target = \"main\"\n[backup.targets.main]\nkind = \"folder\"\npath = {:?}\n",
        path.display().to_string()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_without_a_bundled_layout_is_refused_and_nothing_is_written() {
    let Some(rig) = Rig::new(None).await else {
        println!("SKIPPED: no PostgreSQL (set SPOT_TEST_PG_URL)");
        return;
    };
    // The test site runs on an external server: no layout, however it is asked.
    let run = rig.backup("run", &["--kind", "stopped", "--allow-unencrypted"]);
    let out = text(&run);
    assert_eq!(code(&run), 2, "{out}");
    assert!(out.contains("backup.unsupported"), "{out}");
    assert!(out.contains("does not use the bundled"), "{out}");
    assert!(rig.target_untouched(), "nothing may be written: {out}");

    // Loopback on the bundled port but no installation under the root: the same refusal, and the
    // connection URL (which holds a password) is never quoted.
    rig.use_bundled_server();
    let run = rig.backup("run", &["--kind", "stopped", "--allow-unencrypted"]);
    let out = text(&run);
    assert_eq!(code(&run), 2, "{out}");
    assert!(out.contains("backup.unsupported"), "{out}");
    assert!(!out.contains("t8-secret-pw"), "{out}");
    assert!(rig.target_untouched(), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_inside_the_bundled_server_directories_is_refused() {
    if port_in_use() {
        println!("SKIPPED: something already listens on port {BUNDLED_PORT}");
        return;
    }
    let Some(rig) = Rig::new(None).await else {
        println!("SKIPPED: no PostgreSQL (set SPOT_TEST_PG_URL)");
        return;
    };
    rig.use_bundled_server();
    let base = rig.marker_install();
    // Inside `data_dir` and inside `bin_dir`.
    for inside in [
        base.join("data").join("backups"),
        base.join("bin").join("b"),
    ] {
        std::fs::create_dir_all(&inside).unwrap();
        rig.env.write_common(&["'sample'"], &config_for(&inside));
        rig.use_bundled_server();
        let run = rig.backup("run", &["--kind", "stopped", "--allow-unencrypted"]);
        let out = text(&run);
        assert_eq!(code(&run), 2, "{out}");
        assert!(out.contains("backup.target_unsafe"), "{out}");
        assert!(
            std::fs::read_dir(&inside).unwrap().next().is_none(),
            "{out}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_bundled_instance_is_refused_and_its_files_are_never_copied() {
    let Some(rig) = Rig::new(None).await else {
        println!("SKIPPED: no PostgreSQL (set SPOT_TEST_PG_URL)");
        return;
    };
    rig.use_bundled_server();
    let base = rig.marker_install();

    // A leftover or live postmaster pid file counts as running.
    std::fs::write(base.join("data").join("postmaster.pid"), "1\n").unwrap();
    let run = rig.backup("run", &["--kind", "stopped", "--allow-unencrypted"]);
    let out = text(&run);
    assert_eq!(code(&run), 2, "{out}");
    assert!(out.contains("backup.instance_running"), "{out}");
    assert!(out.contains("nothing was written"), "{out}");
    assert!(rig.target_untouched(), "{out}");
    std::fs::remove_file(base.join("data").join("postmaster.pid")).unwrap();

    // Something answering on the port counts as running too.
    match TcpListener::bind(("127.0.0.1", BUNDLED_PORT)) {
        Ok(_listener) => {
            let run = rig.backup("run", &["--kind", "stopped", "--allow-unencrypted"]);
            let out = text(&run);
            assert_eq!(code(&run), 2, "{out}");
            assert!(out.contains("backup.instance_running"), "{out}");
            assert!(rig.target_untouched(), "{out}");
        }
        Err(_) => println!("SKIPPED the port half: port {BUNDLED_PORT} is already in use"),
    }
}

/// The bundled instance for the end-to-end test, if the environment has one.
fn bundled_root() -> Option<PathBuf> {
    let root = PathBuf::from(
        std::env::var("SPOT_TEST_PG_BIN")
            .ok()
            .filter(|v| !v.is_empty())?,
    );
    (root.join("bin").join("postgres").is_file() && root.join("data").join("PG_VERSION").is_file())
        .then_some(root)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_backup_of_the_bundled_instance_is_written_and_verifies_in_full() {
    let Some(bundled) = bundled_root() else {
        println!(
            "SKIPPED: no bundled layout (set SPOT_TEST_PG_BIN to a root with bin/ and a stopped data/)"
        );
        return;
    };
    if port_in_use() {
        println!("SKIPPED: the bundled instance is running (port {BUNDLED_PORT} is in use)");
        return;
    }
    let Some(rig) = Rig::new(None).await else {
        println!("SKIPPED: no PostgreSQL (set SPOT_TEST_PG_URL)");
        return;
    };
    rig.use_bundled_server();
    std::os::unix::fs::symlink(&bundled, rig.root().join("pgsql")).unwrap();
    std::fs::create_dir_all(rig.root().join("logs")).unwrap();

    let run = rig.backup(
        "run",
        &["--kind", "stopped", "--allow-unencrypted", "--full"],
    );
    let out = text(&run);
    // The downtime is named in the output before the copy starts.
    assert!(
        out.contains("stopped-kind backup: service unavailable for the duration"),
        "{out}"
    );
    // The package exists and is private to its owner. The audit record needs the site database in
    // the bundled instance, which this test does not seed, so the exit may be 3 for that reason
    // alone; any other failure is a failure.
    assert_eq!(rig.packages().len(), 1, "{out}");
    if code(&run) != 0 {
        assert!(out.contains("backup.audit_unavailable"), "{out}");
    }
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&rig.packages()[0]).unwrap().permissions(),
    );
    assert_eq!(mode & 0o077, 0, "package mode {mode:o}");
    assert!(!out.contains("t8-secret-pw"), "{out}");

    // Verify: quick, then full (file checksums of the copy), with the exact wording.
    let quick = rig.backup("verify", &[]);
    assert_eq!(code(&quick), 0, "{}", text(&quick));
    let full = rig.backup("verify", &["--full"]);
    assert_eq!(code(&full), 0, "{}", text(&full));
    assert!(text(&full).contains("verify ok"), "{}", text(&full));
    assert!(!text(&full).contains("t8-secret-pw"));

    // Do not leave the instance the provider restarted running after the test.
    let _ = Command::new(bundled.join("bin").join("pg_ctl"))
        .args(["stop", "-m", "fast", "-D"])
        .arg(bundled.join("data"))
        .output();
}
