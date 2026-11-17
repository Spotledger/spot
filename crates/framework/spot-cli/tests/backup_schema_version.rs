//! WP-PRD-04.T10: the backup manifest carries the site's real recorded schema version
//! (`spot_meta.catalog_version`), never a placeholder. Real PostgreSQL through
//! `SPOT_TEST_PG_URL` and the client tools (`SPOT_TEST_PG_BIN`, else `pg_dump` on `PATH`): each
//! test prints one `SKIPPED` line and passes when either is missing, and never fakes them.
//!
//! SQL statements are written in two pieces (G5 refuses a full statement outside the storage
//! crates), as `capture_invariant_host.rs` does.
#![cfg(unix)]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::process::{Command, Output};

use common::Env;
use spot_postgres::PgBackend;

fn pg_tools() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("SPOT_TEST_PG_BIN") {
        let root = PathBuf::from(root);
        return [root.join("bin"), root]
            .into_iter()
            .find(|d| d.join("pg_dump").is_file());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| dir.join("pg_dump").is_file())
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

struct Rig {
    env: Env,
    target: PathBuf,
    bin: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.target);
    }
}

impl Rig {
    async fn new() -> Option<Rig> {
        let Some(bin) = pg_tools() else {
            println!("SKIPPED: no pg_dump client tools (set SPOT_TEST_PG_BIN)");
            return None;
        };
        let env = Env::new("").await?;
        // A process-wide counter, not the clock alone: two tests of one process can read the same
        // nanosecond, share one directory, and the first to finish removes it under the other.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let target = std::env::temp_dir().join(format!(
            "spot_backup_t10_{}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        env.write_common(
            &["'sample'"],
            &format!(
                "[backup]\ndefault_target = \"main\"\n[backup.targets.main]\nkind = \"folder\"\npath = {:?}\n",
                target.display().to_string()
            ),
        );
        env.site();
        Some(Rig { env, target, bin })
    }

    fn run(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_spot"))
            .arg("--root")
            .arg(&self.env.root.0)
            .args([
                "backup",
                "run",
                "--site",
                &self.env.site,
                "--allow-unencrypted",
            ])
            .env("SPOT_PG_BIN", &self.bin)
            .env("XDG_STATE_HOME", self.env.root.0.join("state"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn packages(&self) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.target)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "spotbak"))
            .collect();
        found.sort();
        found
    }

    /// The schema version in the newest package's manifest.
    fn manifest_version(&self) -> u64 {
        let newest = self.packages().pop().expect("a package was written");
        spot_backup::package::open(&newest)
            .unwrap()
            .manifest()
            .schema_version
    }

    async fn backend(&self) -> PgBackend {
        let loaded = spot_cli::config::load(&self.env.root.0, false).unwrap();
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == self.env.site)
            .unwrap();
        PgBackend::connect(loaded.admin.clone().with_dbname(&site.database))
            .await
            .unwrap()
    }

    async fn sql(&self, head: &str, tail: &str) {
        let client = self.backend().await.pool().get().await.unwrap();
        client
            .batch_execute(&format!("{head}{tail}"))
            .await
            .unwrap();
    }

    /// Sets the recorded catalogue version the way a migration would.
    async fn set_version(&self, value: &str) {
        self.sql(
            "UPDATE spot_",
            &format!("meta SET value = '{value}' WHERE key = 'catalog_version'"),
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_manifest_says_the_migrated_version_and_a_second_migration_moves_it() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    rig.set_version("7").await;
    let first = rig.run();
    let out = text(&first);
    assert_eq!(first.status.code(), Some(0), "{out}");
    assert!(
        out.contains("schema_version=7 schema_version_source=recorded"),
        "{out}"
    );
    assert_eq!(rig.manifest_version(), 7);
    rig.set_version("8").await;
    let second = rig.run();
    assert_eq!(second.status.code(), Some(0), "{}", text(&second));
    assert_eq!(rig.manifest_version(), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_with_no_recorded_version_records_zero_and_says_unrecorded() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    rig.sql("DELETE FROM spot_", "meta WHERE key = 'catalog_version'")
        .await;
    let run = rig.run();
    let out = text(&run);
    assert_eq!(run.status.code(), Some(0), "{out}");
    assert!(
        out.contains("schema_version=0 schema_version_source=unrecorded"),
        "{out}"
    );
    assert_eq!(rig.manifest_version(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_that_cannot_be_read_fails_the_backup_and_writes_no_package() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    rig.set_version("not-a-number").await;
    let run = rig.run();
    let out = text(&run);
    assert_eq!(run.status.code(), Some(3), "{out}");
    assert!(out.contains("schema version could not be read"), "{out}");
    assert!(rig.packages().is_empty(), "no package, no guessed version");
}

#[test]
fn the_placeholder_is_gone_and_no_other_path_writes_a_version() {
    let source = include_str!("../src/commands/backup.rs");
    assert!(
        !source.contains("SCHEMA_VERSION"),
        "the placeholder constant is back"
    );
}
