//! WP-PRD-04.T2: `spot backup` (run, list, verify, prune). Real PostgreSQL through
//! `SPOT_TEST_PG_URL` and the client tools (`SPOT_TEST_PG_BIN`, else `pg_dump` on `PATH`): each
//! test prints one `SKIPPED` line and passes when either is missing, and never fakes them.
#![cfg(unix)]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

/// The command registry, driven without a process (for the injected clock of the retention test).
#[path = "../src/commands/mod.rs"]
mod commands;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use commands::{CommandCtx, Exit};
use common::Env;
use spot_app_api::prelude::RecordId;
use spot_backup::package::{PackageEntry, build_from_files};
use spot_backup::{
    BackupLedger, BackupManifest, BackupRecord, BackupTarget, FileLedger, FixedSpaceProbe,
    FolderTarget, MANIFEST_FORMAT, VerifyMode,
};
use spot_core::ManualClock;
use spot_postgres::PgBackend;
use time::macros::datetime;

/// Full verification names scratch databases by process; tests that make them take turns.
static SCRATCH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

struct Rig {
    env: Env,
    target: PathBuf,
    bin: PathBuf,
    password: String,
    extra: Vec<PathBuf>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        for dir in std::iter::once(&self.target).chain(&self.extra) {
            let _ =
                std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn temp(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spot_backup_t2_{label}_{}", RecordId::new_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config_for(path: &Path, extra: &str) -> String {
    format!(
        "[backup]\ndefault_target = \"main\"\n[backup.targets.main]\nkind = \"folder\"\npath = {:?}\n{extra}\n",
        path.display().to_string()
    )
}

impl Rig {
    /// A site with a folder target `main`; `tools` says whether the PostgreSQL client tools are
    /// needed (a test that only reads packages does not need them).
    async fn new(tools: bool, extra: &str) -> Option<Rig> {
        let bin = match pg_tools() {
            Some(bin) => bin,
            None if tools => {
                println!("SKIPPED: no pg_dump client tools (set SPOT_TEST_PG_BIN)");
                return None;
            }
            None => PathBuf::new(),
        };
        let env = Env::new("").await?;
        let target = temp("target");
        env.write_common(&["'sample'"], &config_for(&target, extra));
        env.site();
        let password = std::env::var("SPOT_TEST_PG_URL")
            .ok()
            .and_then(|u| url::Url::parse(&u).ok())
            .and_then(|u| u.password().map(str::to_owned))
            .unwrap_or_default();
        Some(Rig {
            env,
            target,
            bin,
            password,
            extra: Vec::new(),
        })
    }

    fn site(&self) -> &str {
        &self.env.site
    }

    fn spot(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_spot"))
            .arg("--root")
            .arg(&self.env.root.0)
            .args(args)
            .env("SPOT_PG_BIN", &self.bin)
            .env("XDG_STATE_HOME", self.env.root.0.join("state"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn backup(&self, verb: &str, extra: &[&str]) -> Output {
        let mut args = vec!["backup", verb, "--site", self.site()];
        args.extend_from_slice(extra);
        self.spot(&args)
    }

    /// The packages in the target, oldest first.
    fn packages(&self) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.target)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "spotbak"))
            .collect();
        found.sort();
        found
    }

    fn no_secret_in(&self, outputs: &[&Output]) {
        if self.password.is_empty() {
            return;
        }
        for output in outputs {
            assert!(
                !common::leaks_password(&text(output), &self.password),
                "the password reached the output"
            );
        }
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

    async fn chain_records(&self) -> Vec<String> {
        let client = self.backend().await.pool().get().await.unwrap();
        client
            .query(
                "SELECT record FROM spot_audit_chain WHERE site = $1 AND record LIKE '%backup%' ORDER BY seq",
                &[&format!("identity:{}", self.site())],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect()
    }

    async fn scratch_databases(&self) -> i64 {
        let client = self.backend().await.pool().get().await.unwrap();
        let sql = "SELECT count(*) FROM pg_database WHERE datname LIKE 'spot\\_scratch\\_%'";
        client.query_one(sql, &[]).await.unwrap().get::<_, i64>(0)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_logical_backup_verifies_in_full_and_no_secret_reaches_the_output() {
    // An unencrypted backup is what a site with no key provider (no vault) can take.
    let Some(rig) = Rig::new(true, "# no-secrets\n").await else {
        return;
    };
    let _turn = SCRATCH.lock().await;
    let run = rig.backup("run", &["--full", "--allow-unencrypted"]);
    let out = text(&run);
    assert_eq!(code(&run), 0, "{out}");
    assert!(out.contains("backup ok id="), "{out}");
    assert!(
        out.contains("verify=full") && out.contains("scratch_restore"),
        "{out}"
    );
    assert_eq!(rig.packages().len(), 1);
    let list = rig.backup("list", &[]);
    let listed = text(&list);
    assert_eq!(code(&list), 0, "{listed}");
    assert!(
        listed.contains("verified") && !listed.contains("unverified"),
        "{listed}"
    );
    let verify = rig.backup("verify", &["--full"]);
    assert_eq!(code(&verify), 0, "{}", text(&verify));
    assert!(text(&verify).contains("verify ok"), "{}", text(&verify));
    // The L7 invariants (the shipped checks, the capture invariant, the identity chain) run in
    // a full verification, and the line says which.
    let exact = "verified: snapshot checks + L7 invariants (shipped_checks,capture_invariant,identity_chain)";
    assert!(
        text(&verify).contains(exact) && text(&run).contains(exact),
        "{}",
        text(&verify)
    );
    assert!(out.contains(" unencrypted "), "{out}");
    let records = rig.chain_records().await;
    assert!(
        records
            .iter()
            .any(|r| r.contains("backup_run_unencrypted_override")),
        "the override is audited"
    );
    rig.no_secret_in(&[&run, &list, &verify]);
    assert!(!records.is_empty(), "the runs are in the audit chain");
    if !rig.password.is_empty() {
        assert!(
            records
                .iter()
                .all(|r| !common::leaks_password(r, &rig.password))
        );
    }
    assert_eq!(
        rig.scratch_databases().await,
        0,
        "every scratch database was dropped again"
    );
    // The package is private to its owner.
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&rig.packages()[0]).unwrap().permissions(),
    );
    assert_eq!(mode & 0o077, 0, "package mode {mode:o}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_corrupted_package_fails_verify_and_the_previous_good_backup_survives() {
    let Some(rig) = Rig::new(true, "").await else {
        return;
    };
    for _ in 0..2 {
        let run = rig.backup("run", &["--allow-unencrypted"]);
        assert_eq!(code(&run), 0, "{}", text(&run));
    }
    let packages = rig.packages();
    assert_eq!(packages.len(), 2);
    let (older, newer) = (packages[0].clone(), packages[1].clone());
    let mut bytes = std::fs::read(&newer).unwrap();
    let at = bytes.len() - 10;
    bytes[at] ^= 0xff;
    std::fs::write(&newer, bytes).unwrap();
    let id = newer.file_stem().unwrap().to_string_lossy().into_owned();
    let verify = rig.backup("verify", &["--id", &id]);
    let out = text(&verify);
    assert_eq!(code(&verify), 3, "{out}");
    assert!(
        out.contains("backup.verify_failed") && out.contains("ALERT"),
        "{out}"
    );
    let list = text(&rig.backup("list", &[]));
    assert!(list.contains("unverified"), "{list}");
    let prune = rig.backup("prune", &["--apply"]);
    assert_eq!(code(&prune), 0, "{}", text(&prune));
    assert!(older.exists(), "the previous good backup was not deleted");
    let again = rig.backup(
        "verify",
        &["--id", &older.file_stem().unwrap().to_string_lossy()],
    );
    assert_eq!(code(&again), 0, "{}", text(&again));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_in_the_data_directory_or_behind_a_symlink_is_refused() {
    let Some(rig) = Rig::new(false, "").await else {
        return;
    };
    let inside = rig.env.site_dir().join("backups");
    std::fs::create_dir_all(&inside).unwrap();
    rig.env
        .write_common(&["'sample'"], &config_for(&inside, ""));
    let run = rig.backup("run", &["--allow-unencrypted"]);
    let out = text(&run);
    assert_eq!(code(&run), 2, "{out}");
    assert!(out.contains("backup.target_unsafe"), "{out}");
    assert_eq!(
        std::fs::read_dir(&inside).unwrap().count(),
        0,
        "nothing was written"
    );
    // A symbolic link to a good directory is not followed.
    let link = rig.target.with_extension("link");
    std::os::unix::fs::symlink(&rig.target, &link).unwrap();
    rig.env.write_common(&["'sample'"], &config_for(&link, ""));
    let linked = rig.backup("list", &[]);
    assert_eq!(code(&linked), 2, "{}", text(&linked));
    assert!(
        text(&linked).contains("backup.target_unsafe"),
        "{}",
        text(&linked)
    );
    let _ = std::fs::remove_file(&link);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unwritable_target_leaves_no_file_behind() {
    let Some(rig) = Rig::new(true, "").await else {
        return;
    };
    let permissions = |mode| std::os::unix::fs::PermissionsExt::from_mode(mode);
    std::fs::set_permissions(&rig.target, permissions(0o500)).unwrap();
    if std::fs::File::create(rig.target.join(".probe")).is_ok() {
        std::fs::set_permissions(&rig.target, permissions(0o700)).unwrap();
        println!("SKIPPED: this user can write to a read-only directory");
        return;
    }
    let run = rig.backup("run", &["--allow-unencrypted"]);
    let out = text(&run);
    std::fs::set_permissions(&rig.target, permissions(0o700)).unwrap();
    assert_eq!(code(&run), 2, "{out}");
    assert!(out.contains("backup.target_unavailable"), "{out}");
    assert_eq!(
        std::fs::read_dir(&rig.target).unwrap().count(),
        0,
        "no partial file"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_run_is_refused_while_the_ledger_lock_is_held() {
    let Some(rig) = Rig::new(true, "").await else {
        return;
    };
    let held = FileLedger::lock(&rig.target).unwrap();
    let refused = rig.backup("run", &["--allow-unencrypted"]);
    assert_eq!(code(&refused), 2, "{}", text(&refused));
    assert!(
        text(&refused).contains("backup.ledger_locked"),
        "{}",
        text(&refused)
    );
    assert!(rig.packages().is_empty());
    let leftovers: Vec<_> = std::fs::read_dir(&rig.target)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".work-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    drop(held);
    let run = rig.backup("run", &["--allow-unencrypted"]);
    assert_eq!(code(&run), 0, "{}", text(&run));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_encrypted_backup_names_its_key_and_verifies_in_full() {
    let keys = temp("keys");
    let extra = format!(
        "[services.secrets]\nkek_file = {:?}\n",
        format!("{}/{{site}}.json", keys.display())
    );
    let Some(mut rig) = Rig::new(true, &extra).await else {
        let _ = std::fs::remove_dir_all(&keys);
        return;
    };
    rig.extra.push(keys);
    let _turn = SCRATCH.lock().await;
    let init = rig.spot(&["secrets", "init", "--site", rig.site(), "--apply"]);
    assert_eq!(code(&init), 0, "{}", text(&init));
    let run = rig.backup("run", &["--full"]);
    let out = text(&run);
    assert_eq!(code(&run), 0, "{out}");
    assert!(
        out.contains(" encrypted ") && !out.contains("unencrypted"),
        "{out}"
    );
    let header =
        String::from_utf8_lossy(&std::fs::read(&rig.packages()[0]).unwrap()[..2048]).into_owned();
    assert!(header.contains("encryption"), "the manifest names the key");
    let verify = rig.backup("verify", &["--full"]);
    assert_eq!(code(&verify), 0, "{}", text(&verify));
    rig.no_secret_in(&[&run, &verify]);
    let records = rig.chain_records().await;
    assert!(
        records.iter().all(|r| !r.contains("unencrypted_override")),
        "no override was needed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_without_a_key_provider_is_refused_unless_unencrypted_is_allowed() {
    // The sign-in stores need a vault, which is also the key provider: this site is the one
    // that has none.
    let Some(rig) = Rig::new(true, "# no-secrets\n").await else {
        return;
    };
    let refused = rig.backup("run", &[]);
    let out = text(&refused);
    assert_eq!(code(&refused), 2, "{out}");
    assert!(out.contains("backup.encryption_unavailable"), "{out}");
    assert!(rig.packages().is_empty(), "nothing was written");
    let allowed = rig.backup("run", &["--allow-unencrypted"]);
    let out = text(&allowed);
    assert_eq!(code(&allowed), 0, "{out}");
    assert!(
        out.contains(" unencrypted ") && out.contains("--allow-unencrypted was given"),
        "{out}"
    );
    let header =
        String::from_utf8_lossy(&std::fs::read(&rig.packages()[0]).unwrap()[..1024]).into_owned();
    assert!(
        !header.contains("[encryption]"),
        "the manifest records no key"
    );
    let records = rig.chain_records().await;
    assert!(
        records
            .iter()
            .any(|r| r.contains("backup_run_unencrypted_override")),
        "the override is audited"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_errors_and_unsupported_choices_are_refused_before_anything_runs() {
    let Some(rig) = Rig::new(false, "").await else {
        return;
    };
    let none = rig.spot(&["backup", "list"]);
    assert_eq!(code(&none), 1, "{}", text(&none));
    let target = rig.backup("list", &["--target", "nowhere"]);
    assert_eq!(code(&target), 2, "{}", text(&target));
    let stopped = rig.backup("run", &["--kind", "stopped"]);
    assert_eq!(code(&stopped), 2, "{}", text(&stopped));
    assert!(text(&stopped).contains("backup.unsupported"));
    let kind = rig.backup("run", &["--kind", "weekly"]);
    assert_eq!(code(&kind), 1, "{}", text(&kind));
    let id = rig.backup("verify", &["--id", "../../etc/passwd"]);
    assert_eq!(code(&id), 1, "{}", text(&id));
}

// --- retention with an injected clock ---------------------------------------------------------

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn backup_cmd(root: &Path, args: &[&str]) -> (Exit, String) {
    let (out, err) = (Sink::default(), Sink::default());
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-30 09:00:00 UTC)));
    let mut ctx = CommandCtx::new(
        root.to_path_buf(),
        clock,
        Box::new(out.clone()),
        Box::new(err.clone()),
    );
    let registration = commands::COMMANDS
        .iter()
        .find(|r| r.name == "backup")
        .expect("the backup command registers itself");
    let mut argv = vec!["backup"];
    argv.extend_from_slice(args);
    let matches = (registration.command)()
        .try_get_matches_from(argv)
        .expect("the arguments parse");
    let exit = match (registration.run)(&matches, &mut ctx).await {
        Ok(exit) => exit,
        Err(e) => {
            let _ = writeln!(ctx.err(), "{e}");
            Exit::Failed
        }
    };
    let both = [out.0.lock().unwrap().clone(), err.0.lock().unwrap().clone()].concat();
    (exit, String::from_utf8_lossy(&both).into_owned())
}

/// A stored backup of `tenant` made on `day` of November 2026, recorded verified.
fn seed(dir: &Path, tenant: &str, n: u64, day: u32) {
    let id = format!("{tenant}-main-{n}");
    let manifest = BackupManifest {
        format: MANIFEST_FORMAT.to_owned(),
        id: id.clone(),
        tenant: tenant.to_owned(),
        created_at: format!("2026-11-{day:02}T02:00:00Z"),
        platform_version: "0.1.0".to_owned(),
        snapshot_provider_version: "0.1.0".to_owned(),
        pg_major: 18,
        schema_version: 1,
        app_set: Vec::new(),
        digest: "0".repeat(64),
        size: 0,
        file_count: 0,
        chain_id: "main".to_owned(),
        chain_sequence: n,
        parent_digest: None,
        verify: VerifyMode::Quick,
        target: "folder:test".to_owned(),
        encryption: None,
    };
    let created = manifest.created_at().unwrap();
    let work = dir.join(format!(".seed-{n}"));
    std::fs::create_dir_all(&work).unwrap();
    let entry = PackageEntry {
        path: "data".to_owned(),
        bytes: vec![u8::try_from(n).unwrap()],
    };
    let built = build_from_files(manifest, vec![entry], &work).unwrap();
    let (size, digest) = built.package.stream_facts().unwrap();
    let target = FolderTarget::new(dir, Arc::new(FixedSpaceProbe(1 << 30)));
    target
        .put(&id, built.package.reader().unwrap(), size, &digest)
        .unwrap();
    std::fs::remove_dir_all(&work).unwrap();
    let record = BackupRecord {
        id,
        created_at: created,
        verified: true,
        sequence: n,
        open_restore: false,
    };
    FileLedger::open(dir).unwrap().try_upsert(record).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_keeps_the_policys_set_and_never_the_last_good_backup_removed() {
    let retention =
        "[backup.retention]\nkeep_daily = 2\nkeep_weekly = 0\nkeep_monthly = 0\nkeep_last = 1\n";
    let Some(rig) = Rig::new(false, retention).await else {
        return;
    };
    for (n, day) in [(1, 10), (2, 11), (3, 12), (4, 13), (5, 14)] {
        seed(&rig.target, rig.site(), n, day);
    }
    let site = rig.site().to_owned();
    let tenant = |n: u64| format!("{site}-main-{n}");
    let (exit, out) = backup_cmd(&rig.env.root.0, &["prune", "--site", &site]).await;
    assert_eq!(exit.code(), 0, "{out}");
    for n in [1, 2, 3] {
        assert!(out.contains(&format!("remove {}", tenant(n))), "{out}");
    }
    for n in [4, 5] {
        assert!(out.contains(&format!("keep {}", tenant(n))), "{out}");
        assert!(!out.contains(&format!("remove {}", tenant(n))), "{out}");
    }
    assert_eq!(rig.packages().len(), 5, "a dry run removes nothing");
    let (exit, out) = backup_cmd(&rig.env.root.0, &["prune", "--site", &site, "--apply"]).await;
    assert_eq!(exit.code(), 0, "{out}");
    let left: Vec<String> = rig
        .packages()
        .iter()
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, vec![tenant(4), tenant(5)]);
    let ledger = FileLedger::open(&rig.target).unwrap();
    assert_eq!(ledger.last_verified().unwrap().id, tenant(5));
    assert_eq!(ledger.records().len(), 2);
}
