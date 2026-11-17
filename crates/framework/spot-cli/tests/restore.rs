//! WP-PRD-04.T3: `spot restore` (preflight, apply, purge-quarantine). Real PostgreSQL through
//! `SPOT_TEST_PG_URL` and the client tools (`SPOT_TEST_PG_BIN`, else `pg_dump` and `pg_restore` on
//! `PATH`): each test prints one `SKIPPED` line and passes when either is missing, and never
//! fakes them. The kill tests need a debug build (the fault seam is compiled out of release).
#![cfg(unix)]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use common::Env;
use spot_app_api::prelude::RecordId;
use spot_backup::package::{PackageEntry, build_from_files};
use spot_backup::{
    BackupLedger, BackupManifest, BackupRecord, BackupTarget, FileLedger, FixedSpaceProbe,
    FolderTarget, MANIFEST_FORMAT, VerifyMode,
};
use spot_postgres::PgBackend;

/// Restores and scratch databases are named by process and second; tests take turns.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn pg_tools() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("SPOT_TEST_PG_BIN") {
        let root = PathBuf::from(root);
        return [root.join("bin"), root]
            .into_iter()
            .find(|d| d.join("pg_dump").is_file() && d.join("pg_restore").is_file());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .find(|d| d.join("pg_dump").is_file() && d.join("pg_restore").is_file())
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

fn temp(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spot_restore_t3_{label}_{}", RecordId::new_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    env: Env,
    target: PathBuf,
    bin: PathBuf,
    password: String,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.target);
    }
}

impl Rig {
    async fn new() -> Option<Rig> {
        let Some(bin) = pg_tools() else {
            println!("SKIPPED: no pg_dump and pg_restore client tools (set SPOT_TEST_PG_BIN)");
            return None;
        };
        let env = Env::new("").await?;
        let target = temp("target");
        let config = format!(
            "[backup]\ndefault_target = \"main\"\n[backup.targets.main]\nkind = \"folder\"\npath = {:?}\n",
            target.display().to_string()
        );
        env.write_common(&["'sample'"], &config);
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
        })
    }

    fn site(&self) -> &str {
        &self.env.site
    }

    fn db(&self) -> String {
        self.env.site.replace('-', "_")
    }

    fn root(&self) -> &Path {
        &self.env.root.0
    }

    fn spot_with(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_spot"));
        command
            .arg("--root")
            .arg(self.root())
            .args(args)
            .env("SPOT_PG_BIN", &self.bin)
            .env("XDG_STATE_HOME", self.root().join("state"))
            .stdin(std::process::Stdio::null());
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn spot(&self, args: &[&str]) -> Output {
        self.spot_with(args, &[])
    }

    /// One backup of the site; its id.
    fn backup(&self) -> String {
        let run = self.spot(&[
            "backup",
            "run",
            "--site",
            self.site(),
            "--allow-unencrypted",
        ]);
        let out = text(&run);
        assert_eq!(code(&run), Some(0), "{out}");
        out.split_whitespace()
            .find_map(|w| w.strip_prefix("id="))
            .unwrap()
            .to_owned()
    }

    fn preflight(&self, id: &str, extra: &[&str]) -> Output {
        let mut args = vec!["restore", "preflight", "--site", self.site(), "--from", id];
        args.extend_from_slice(extra);
        self.spot(&args)
    }

    fn token_of(output: &Output) -> String {
        text(output)
            .lines()
            .find_map(|l| l.strip_prefix("confirm_token="))
            .and_then(|l| l.split_whitespace().next())
            .unwrap_or_else(|| panic!("no confirm token in {}", text(output)))
            .to_owned()
    }

    fn apply_with(&self, id: &str, token: &str, extra: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut args = vec![
            "restore",
            "apply",
            "--site",
            self.site(),
            "--from",
            id,
            "--confirm",
            token,
            "--accept-unverified-schema",
        ];
        args.extend_from_slice(extra);
        self.spot_with(&args, envs)
    }

    /// A fresh token and the restore of `id`.
    fn restore(&self, id: &str) -> Output {
        let pf = self.preflight(id, &[]);
        assert_eq!(code(&pf), Some(0), "{}", text(&pf));
        self.apply_with(id, &Self::token_of(&pf), &[], &[])
    }

    async fn backend(&self, database: &str) -> PgBackend {
        let loaded = spot_cli::config::load(self.root(), false).unwrap();
        PgBackend::connect(loaded.admin.clone().with_dbname(database))
            .await
            .unwrap()
    }

    /// The first column of every row of `sql` on `database`, as text.
    async fn q(&self, database: &str, sql: &str) -> Vec<String> {
        let client = self.backend(database).await.pool().get().await.unwrap();
        client
            .query(sql, &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect()
    }

    async fn x(&self, database: &str, sql: &str) {
        let client = self.backend(database).await.pool().get().await.unwrap();
        client.batch_execute(sql).await.unwrap();
    }

    /// The site's gate: `open` when it is open or no control schema exists yet.
    async fn gate_state(&self) -> String {
        let db = self.db();
        let present = self
            .q(
                &db,
                "SELECT (to_regclass('spot_migration_control.gate') IS NOT NULL)::text",
            )
            .await;
        if present != vec!["true".to_owned()] {
            return "open".to_owned();
        }
        let rows = self
            .q(
                &db,
                "SELECT state::text FROM spot_migration_control.gate LIMIT 1",
            )
            .await;
        rows.first().cloned().unwrap_or_else(|| "open".to_owned())
    }

    async fn probe(&self) -> String {
        self.q(&self.db(), "SELECT v::text FROM restore_probe")
            .await
            .join(",")
    }

    async fn seed_probe(&self, value: &str) {
        let db = self.db();
        self.x(&db, "CREATE TABLE IF NOT EXISTS restore_probe (v text)")
            .await;
        self.x(
            &db,
            &format!("DELETE FROM restore_probe; INSERT INTO restore_probe VALUES ('{value}')"),
        )
        .await;
    }

    /// Databases of this site other than the live one (quarantines, restored copies, scratches).
    async fn others(&self) -> Vec<String> {
        let sql = format!(
            "SELECT datname::text FROM pg_database WHERE datname LIKE '{p}%' AND datname <> '{db}' OR datname LIKE 'spot\\_scratch\\_rs\\_%' ORDER BY 1",
            p = self
                .db()
                .chars()
                .take(40)
                .collect::<String>()
                .replace('_', "\\_"),
            db = self.db()
        );
        self.q("postgres", &sql).await
    }

    async fn drop_database(&self, name: &str) {
        let sql = format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)");
        self.x("postgres", &sql).await;
    }

    async fn cleanup(&self) {
        for name in self.others().await {
            self.drop_database(&name).await;
        }
    }

    fn no_secret_in(&self, outputs: &[&Output]) {
        if self.password.is_empty() {
            return;
        }
        for output in outputs {
            assert!(
                !text(output).contains(&self.password),
                "the password reached the output"
            );
        }
    }

    fn token_file(&self) -> PathBuf {
        self.root()
            .join("sites")
            .join(self.site())
            .join("state")
            .join("restore-confirm.toml")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restore_brings_the_original_data_back_and_keeps_what_it_replaced() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    let _turn = TURN.lock().await;
    rig.seed_probe("before").await;
    let id = rig.backup();
    rig.x(&rig.db(), "UPDATE restore_probe SET v = 'after'")
        .await;
    let pf = rig.preflight(&id, &[]);
    let out = text(&pf);
    assert_eq!(code(&pf), Some(0), "{out}");
    assert!(out.contains("replaces=database"), "{out}");
    assert!(out.contains("package=verified"), "{out}");
    assert!(
        out.contains("schema_version: unverified"),
        "the placeholder is unverified: {out}"
    );
    let token = Rig::token_of(&pf);
    assert_eq!(rig.probe().await, "after", "preflight changed nothing");
    // Without the explicit flag, an unverified schema is refused and the token is kept.
    let plain = rig.spot(&[
        "restore",
        "apply",
        "--site",
        rig.site(),
        "--from",
        &id,
        "--confirm",
        &token,
    ]);
    assert_eq!(code(&plain), Some(2), "{}", text(&plain));
    assert!(
        text(&plain).contains("restore.schema_unverified"),
        "{}",
        text(&plain)
    );
    assert!(
        rig.token_file().exists(),
        "a refusal does not spend the token"
    );
    assert_eq!(rig.probe().await, "after");
    let apply = rig.apply_with(&id, &token, &[], &[]);
    let done = text(&apply);
    assert_eq!(code(&apply), Some(0), "{done}");
    assert!(done.contains("restore ok"), "{done}");
    assert!(done.contains("unverified, accepted"), "{done}");
    assert_eq!(rig.probe().await, "before", "the original data is back");
    // The replaced database is kept, with the data it held.
    let others = rig.others().await;
    assert_eq!(others.len(), 1, "one quarantine and no scratch: {others:?}");
    assert!(others[0].contains("_replaced_"), "{others:?}");
    let kept = rig.q(&others[0], "SELECT v::text FROM restore_probe").await;
    assert_eq!(kept, vec!["after".to_owned()]);
    // The gate is open again and the audit chain has the story.
    assert_eq!(rig.gate_state().await, "open");
    let records = rig
        .q(
            &rig.db(),
            "SELECT record::text FROM spot_audit_chain WHERE record LIKE '%restore_succeeded%'",
        )
        .await;
    assert!(
        !records.is_empty(),
        "restore_succeeded is in the restored database's chain"
    );
    rig.no_secret_in(&[&pf, &plain, &apply]);
    assert!(!done.contains(&token) && !records.iter().any(|r| r.contains(&token)));
    rig.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_reused_or_foreign_token_is_refused() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    let _turn = TURN.lock().await;
    rig.seed_probe("before").await;
    let first = rig.backup();
    let second = rig.backup();
    rig.x(&rig.db(), "UPDATE restore_probe SET v = 'after'")
        .await;
    let pf = rig.preflight(&first, &[]);
    let token = Rig::token_of(&pf);
    // A token of the right shape that is not the issued one.
    let wrong = rig.apply_with(&first, &"A".repeat(43), &[], &[]);
    assert_eq!(code(&wrong), Some(2), "{}", text(&wrong));
    assert!(
        text(&wrong).contains("restore.confirm_invalid"),
        "{}",
        text(&wrong)
    );
    // The right token for another backup.
    let other = rig.apply_with(&second, &token, &[], &[]);
    assert_eq!(code(&other), Some(2), "{}", text(&other));
    assert!(
        text(&other).contains("restore.confirm_invalid"),
        "{}",
        text(&other)
    );
    assert_eq!(rig.probe().await, "after", "nothing was restored");
    // The right token once, then never again.
    let good = rig.apply_with(&first, &token, &[], &[]);
    assert_eq!(code(&good), Some(0), "{}", text(&good));
    let again = rig.apply_with(&first, &token, &[], &[]);
    assert_eq!(code(&again), Some(2), "{}", text(&again));
    assert!(
        text(&again).contains("restore.confirm_invalid"),
        "{}",
        text(&again)
    );
    // A token of this site does not work for another site.
    let other_site = format!("{}-b", rig.site());
    common::ok(rig.root(), &["new-site", &other_site]);
    let pf = rig.preflight(&second, &[]);
    let token = Rig::token_of(&pf);
    let foreign = rig.spot(&[
        "restore",
        "apply",
        "--site",
        &other_site,
        "--from",
        &second,
        "--confirm",
        &token,
        "--accept-unverified-schema",
    ]);
    assert_eq!(code(&foreign), Some(2), "{}", text(&foreign));
    rig.cleanup().await;
    let other_db = other_site.replace('-', "_");
    rig.drop_database(&other_db).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tampered_package_is_refused_before_the_gate_is_entered() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    let _turn = TURN.lock().await;
    rig.seed_probe("before").await;
    let id = rig.backup();
    rig.x(&rig.db(), "UPDATE restore_probe SET v = 'after'")
        .await;
    let pf = rig.preflight(&id, &[]);
    let token = Rig::token_of(&pf);
    let package = std::fs::read_dir(&rig.target)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "spotbak"))
        .unwrap();
    let mut bytes = std::fs::read(&package).unwrap();
    let at = bytes.len() - 10;
    bytes[at] ^= 0xff;
    std::fs::write(&package, bytes).unwrap();
    let apply = rig.apply_with(&id, &token, &[], &[]);
    let out = text(&apply);
    assert_eq!(code(&apply), Some(2), "{out}");
    assert!(out.contains("backup.verify_failed"), "{out}");
    assert!(out.contains("gate was not touched"), "{out}");
    assert_eq!(rig.probe().await, "after");
    assert_eq!(rig.gate_state().await, "open", "the gate was never closed");
    assert!(
        rig.token_file().exists(),
        "a refusal does not spend the token"
    );
    // A tampered package does not preflight either.
    let again = rig.preflight(&id, &[]);
    assert_eq!(code(&again), Some(2), "{}", text(&again));
    assert!(
        text(&again).contains("backup.verify_failed"),
        "{}",
        text(&again)
    );
    rig.cleanup().await;
}

/// A stored backup of `tenant` with schema version `schema` and one small payload file.
fn seed(dir: &Path, tenant: &str, n: u64, schema: u64) -> String {
    let id = format!("{tenant}-main-{n}");
    let manifest = BackupManifest {
        format: MANIFEST_FORMAT.to_owned(),
        id: id.clone(),
        tenant: tenant.to_owned(),
        created_at: "2026-11-10T02:00:00Z".to_owned(),
        platform_version: "0.1.0".to_owned(),
        snapshot_provider_version: "0.1.0".to_owned(),
        pg_major: 18,
        schema_version: schema,
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
        bytes: vec![1],
    };
    let built = build_from_files(manifest, vec![entry], &work).unwrap();
    let (size, digest) = built.package.stream_facts().unwrap();
    let target = FolderTarget::new(dir, Arc::new(FixedSpaceProbe(1 << 30)));
    target
        .put(&id, built.package.reader().unwrap(), size, &digest)
        .unwrap();
    std::fs::remove_dir_all(&work).unwrap();
    FileLedger::open(dir)
        .unwrap()
        .try_upsert(BackupRecord {
            id: id.clone(),
            created_at: created,
            verified: true,
            sequence: n,
            open_restore: false,
        })
        .unwrap();
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_backup_of_another_site_or_a_newer_schema_is_refused() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    let _turn = TURN.lock().await;
    let foreign = seed(&rig.target, "someone-else", 1, 1);
    let refused = rig.preflight(&foreign, &[]);
    assert_eq!(code(&refused), Some(2), "{}", text(&refused));
    assert!(
        text(&refused).contains("never restored into another site"),
        "{}",
        text(&refused)
    );
    assert!(!rig.token_file().exists(), "no token for a refused restore");
    let newer = seed(&rig.target, rig.site(), 2, 999_999);
    let too_new = rig.preflight(&newer, &[]);
    assert_eq!(code(&too_new), Some(2), "{}", text(&too_new));
    assert!(
        text(&too_new).contains("restore.too_new"),
        "{}",
        text(&too_new)
    );
    // The record of the refusal is in the audit chain.
    let records = rig
        .q(
            &rig.db(),
            "SELECT record::text FROM spot_audit_chain WHERE record LIKE '%restore_refused%'",
        )
        .await;
    assert!(!records.is_empty(), "refusals are audited");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_between_steps_the_previous_database_is_intact_and_the_next_run_recovers() {
    if !cfg!(debug_assertions) {
        println!("SKIPPED: the fault seam exists only in debug builds");
        return;
    }
    let Some(rig) = Rig::new().await else {
        return;
    };
    let _turn = TURN.lock().await;
    rig.seed_probe("before").await;
    let id = rig.backup();
    rig.x(&rig.db(), "UPDATE restore_probe SET v = 'after'")
        .await;
    for point in ["after_gate", "after_scratch", "before_swap"] {
        let pf = rig.preflight(&id, &[]);
        assert_eq!(code(&pf), Some(0), "{point}: {}", text(&pf));
        let killed = rig.apply_with(
            &id,
            &Rig::token_of(&pf),
            &[],
            &[("SPOT_RESTORE_FAULT", point)],
        );
        assert_ne!(
            code(&killed),
            Some(0),
            "{point}: the process was killed: {}",
            text(&killed)
        );
        // The previous database is there, with its data, though it bars connections.
        let allow = rig
            .q(
                "postgres",
                &format!(
                    "SELECT datallowconn::text FROM pg_database WHERE datname = '{}'",
                    rig.db()
                ),
            )
            .await;
        assert_eq!(
            allow,
            vec!["false".to_owned()],
            "{point}: the live database is fenced; the killed run said: {}",
            text(&killed)
        );
        // The next run notices, reopens and says so; the data is intact.
        let next = rig.preflight(&id, &[]);
        let out = text(&next);
        assert_eq!(code(&next), Some(0), "{point}: {out}");
        assert!(out.contains("did not finish"), "{point}: {out}");
        assert_eq!(
            rig.probe().await,
            "after",
            "{point}: the previous database is intact"
        );
        assert_eq!(
            rig.gate_state().await,
            "open",
            "{point}: the gate is open again"
        );
        rig.cleanup().await;
    }
    // After all that, an ordinary restore works.
    let done = rig.restore(&id);
    assert_eq!(code(&done), Some(0), "{}", text(&done));
    assert_eq!(rig.probe().await, "before");
    rig.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn as_new_site_restores_into_a_new_site_and_refuses_an_existing_name() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    let _turn = TURN.lock().await;
    rig.seed_probe("before").await;
    let id = rig.backup();
    rig.x(&rig.db(), "UPDATE restore_probe SET v = 'after'")
        .await;
    // An existing site's name is refused, in preflight and in apply.
    let taken = rig.preflight(&id, &["--as-new-site", rig.site()]);
    assert_eq!(code(&taken), Some(2), "{}", text(&taken));
    assert!(
        text(&taken).contains("restore.target_exists"),
        "{}",
        text(&taken)
    );
    let name = format!("{}-copy", rig.site());
    let pf = rig.preflight(&id, &["--as-new-site", &name]);
    assert_eq!(code(&pf), Some(0), "{}", text(&pf));
    assert!(text(&pf).contains("is not touched"), "{}", text(&pf));
    // A token for the new site does not serve a restore over the site.
    let over = rig.apply_with(&id, &Rig::token_of(&pf), &[], &[]);
    assert_eq!(code(&over), Some(2), "{}", text(&over));
    assert_eq!(rig.probe().await, "after");
    let apply = rig.apply_with(&id, &Rig::token_of(&pf), &["--as-new-site", &name], &[]);
    assert_eq!(code(&apply), Some(0), "{}", text(&apply));
    assert!(
        rig.root()
            .join("sites")
            .join(&name)
            .join("site.toml")
            .is_file()
    );
    let new_db = name.replace('-', "_");
    let copied = rig.q(&new_db, "SELECT v::text FROM restore_probe").await;
    assert_eq!(copied, vec!["before".to_owned()]);
    assert_eq!(rig.probe().await, "after", "the source site is untouched");
    assert!(
        rig.others().await.is_empty() || rig.others().await == vec![new_db.clone()],
        "no quarantine, no scratch: {:?}",
        rig.others().await
    );
    rig.drop_database(&new_db).await;
    rig.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_local_state_is_reset_by_a_restore() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    let _turn = TURN.lock().await;
    let db = rig.db();
    rig.seed_probe("before").await;
    rig.x(
        &db,
        "CREATE TABLE IF NOT EXISTS spot_coord_lease (name text PRIMARY KEY, holder text NOT NULL, \
         token bigint NOT NULL, expires_at timestamptz NOT NULL, lease_id bigint NOT NULL); \
         DELETE FROM spot_coord_lease; \
         INSERT INTO spot_coord_lease VALUES ('jobs', 'node-that-is-gone', 7, now() + interval '1 hour', 1)",
    )
    .await;
    let id = rig.backup();
    let done = rig.restore(&id);
    assert_eq!(code(&done), Some(0), "{}", text(&done));
    let held = rig
        .q(&db, "SELECT count(*)::text FROM spot_coord_lease")
        .await;
    assert_eq!(
        held,
        vec!["0".to_owned()],
        "leases of the backed-up node do not survive a restore"
    );
    assert_eq!(rig.probe().await, "before");
    rig.cleanup().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn purge_quarantine_never_drops_a_young_one_and_drops_an_old_one_only_with_apply() {
    let Some(rig) = Rig::new().await else {
        return;
    };
    let _turn = TURN.lock().await;
    let head: String = rig.db().chars().take(40).collect();
    let (old, young) = (format!("{head}_replaced_1"), format!("{head}_replaced_2"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for (name, secs) in [(&old, 1), (&young, now - 100)] {
        let marker = format!("spot-scratch:v1:quarantine:{}:{secs}", rig.site());
        rig.x("postgres", &format!("CREATE DATABASE \"{name}\""))
            .await;
        rig.x(
            "postgres",
            &format!("COMMENT ON DATABASE \"{name}\" IS '{marker}'"),
        )
        .await;
    }
    let too_young = rig.spot(&[
        "restore",
        "purge-quarantine",
        "--site",
        rig.site(),
        "--older-than",
        "1h",
        "--apply",
    ]);
    assert_eq!(code(&too_young), Some(2), "{}", text(&too_young));
    assert!(
        text(&too_young).contains("restore.quarantine_young"),
        "{}",
        text(&too_young)
    );
    let dry = rig.spot(&[
        "restore",
        "purge-quarantine",
        "--site",
        rig.site(),
        "--older-than",
        "7d",
    ]);
    let out = text(&dry);
    assert_eq!(code(&dry), Some(0), "{out}");
    assert!(
        out.contains(&format!("would drop {old}")) && out.contains(&format!("keep {young}")),
        "{out}"
    );
    assert_eq!(rig.others().await.len(), 2, "a dry run drops nothing");
    let apply = rig.spot(&[
        "restore",
        "purge-quarantine",
        "--site",
        rig.site(),
        "--older-than",
        "7d",
        "--apply",
    ]);
    assert_eq!(code(&apply), Some(0), "{}", text(&apply));
    assert_eq!(
        rig.others().await,
        vec![young.clone()],
        "only the old one was dropped"
    );
    rig.cleanup().await;
}
