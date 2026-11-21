//! WP-PRD-04.T12: `spot backup verify --full` (and `run --full`) run the L7 invariants over the
//! scratch restore: the journal capture invariant and the identity audit chain, next to the shipped
//! snapshot checks. Real PostgreSQL through `SPOT_TEST_PG_URL` and the client tools
//! (`SPOT_TEST_PG_BIN`, else `pg_dump` on `PATH`): each test prints one `SKIPPED` line and passes
//! when either is missing, and never fakes them.
//!
//! SQL statements are written in two pieces (G5 refuses a full statement outside the storage
//! crates), as `capture_invariant_host.rs` does.
#![cfg(unix)]
#![allow(missing_docs, dead_code, clippy::unwrap_used, clippy::expect_used)]

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

impl Rig {
    fn spot(&self, args: &[&str]) -> Output {
        let mut full = vec!["backup"];
        full.extend_from_slice(args);
        Command::new(env!("CARGO_BIN_EXE_spot"))
            .arg("--root")
            .arg(&self.env.root.0)
            .args(full)
            .args(["--site", &self.env.site])
            .env("SPOT_PG_BIN", &self.bin)
            .env("XDG_STATE_HOME", self.env.root.0.join("state"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    async fn scratch_databases(&self) -> i64 {
        let client = self.backend().await.pool().get().await.unwrap();
        let sql = format!(
            "{}{}",
            "SELECT count(*) FROM pg_data", "base WHERE datname LIKE 'spot\\_scratch\\_%'"
        );
        client.query_one(&sql, &[]).await.unwrap().get::<_, i64>(0)
    }

    /// How many entries the site's identity chain holds.
    async fn chain_entries(&self) -> i64 {
        let client = self.backend().await.pool().get().await.unwrap();
        let sql = format!(
            "{}{}",
            "SELECT count(*) FROM spot_audit", "_chain WHERE site = $1"
        );
        client
            .query_one(&sql, &[&format!("identity:{}", self.env.site)])
            .await
            .unwrap()
            .get::<_, i64>(0)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_verification_lists_the_invariants_it_ran_and_leaves_no_scratch_database() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    let run = rig.spot(&["run", "--allow-unencrypted", "--full"]);
    let out = text(&run);
    assert_eq!(run.status.code(), Some(0), "{out}");
    assert!(
        out.contains("verified: snapshot checks + L7 invariants (shipped_checks,capture_invariant,identity_chain)"),
        "{out}"
    );
    assert!(!out.contains("L7 invariants not run"), "{out}");
    let verify = rig.spot(&["verify", "--full"]);
    let out = text(&verify);
    assert_eq!(verify.status.code(), Some(0), "{out}");
    assert!(out.contains("l7_invariants"), "{out}");
    assert_eq!(
        rig.scratch_databases().await,
        0,
        "the scratch database was dropped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_identity_chain_fails_the_full_verification_and_the_ledger_shows_it_unverified() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    if rig.chain_entries().await == 0 {
        println!("SKIPPED: the site's identity chain has no entry to break");
        return;
    }
    // Change the first entry's record in place, before the backup: the dump carries the break.
    {
        let client = rig.backend().await.pool().get().await.unwrap();
        let sql = format!(
            "{}{}",
            "UPDATE spot_audit", "_chain SET record = record || ' ' WHERE site = $1 AND seq = 1"
        );
        client
            .execute(&sql, &[&format!("identity:{}", rig.env.site)])
            .await
            .unwrap();
    }
    let run = rig.spot(&["run", "--allow-unencrypted"]);
    assert_eq!(
        run.status.code(),
        Some(0),
        "the quick run does not look: {}",
        text(&run)
    );
    let verify = rig.spot(&["verify", "--full"]);
    let out = text(&verify);
    assert_eq!(verify.status.code(), Some(3), "{out}");
    assert!(
        out.contains("backup.invariants_failed") || out.contains("identity_chain"),
        "{out}"
    );
    assert!(
        !rig.packages().is_empty(),
        "the package is kept for inspection"
    );
    let list = rig.spot(&["list"]);
    assert!(text(&list).contains("unverified"), "{}", text(&list));
    assert_eq!(
        rig.scratch_databases().await,
        0,
        "the scratch database was dropped on failure too"
    );
    // The output names kinds and sequence numbers, never row data.
    assert!(!out.contains("record"), "{out}");
}
