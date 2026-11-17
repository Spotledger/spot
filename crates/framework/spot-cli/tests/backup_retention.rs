//! WP-PRD-04.T11: `[backup.retention]` goes to the library planner whole (all four numbers), and
//! `spot backup prune` removes exactly what that plan lists. A real folder target, an injected
//! clock and real PostgreSQL through `SPOT_TEST_PG_URL` (each test prints one `SKIPPED` line and
//! passes when it is unset, and never fakes it).
//!
//! "A failed run deletes nothing" is a property of the library runner (it applies retention only
//! after the new backup verified): `spot-backup`'s `backup_flow` tests pin it; this file pins the
//! command's side.
#![cfg(unix)]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

/// The command registry, driven without a process (for the injected clock).
#[path = "../src/commands/mod.rs"]
mod commands;

use std::io::Write;
use std::path::{Path, PathBuf};
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
use time::macros::datetime;

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
    async fn new(retention: &str) -> Option<Rig> {
        let env = Env::new("").await?;
        let target = std::env::temp_dir().join(format!("spot_backup_t11_{}", RecordId::new_v7()));
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        env.write_common(
            &["'sample'"],
            &format!(
                "[backup]\ndefault_target = \"main\"\n[backup.targets.main]\nkind = \"folder\"\npath = {:?}\n{retention}\n",
                target.display().to_string()
            ),
        );
        env.site();
        Some(Rig { env, target })
    }

    fn site(&self) -> &str {
        &self.env.site
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
}

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

fn password() -> String {
    std::env::var("SPOT_TEST_PG_URL")
        .ok()
        .and_then(|u| url::Url::parse(&u).ok())
        .and_then(|u| u.password().map(str::to_owned))
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_last_five_of_eight_keeps_five_and_a_dry_run_deletes_nothing() {
    let retention =
        "[backup.retention]\nkeep_daily = 1\nkeep_weekly = 0\nkeep_monthly = 0\nkeep_last = 5\n";
    let Some(rig) = Rig::new(retention).await else {
        return;
    };
    for n in 1..=8_u64 {
        seed(&rig.target, rig.site(), n, u32::try_from(n).unwrap());
    }
    let site = rig.site().to_owned();
    let id = |n: u64| format!("{site}-main-{n}");
    let (exit, out) = backup_cmd(&rig.env.root.0, &["prune", "--site", &site]).await;
    assert_eq!(exit.code(), 0, "{out}");
    for n in 4..=8 {
        assert!(out.contains(&format!("keep {}", id(n))), "{out}");
    }
    for n in 1..=3 {
        assert!(out.contains(&format!("remove {}", id(n))), "{out}");
    }
    assert!(out.contains("dry run: 3 backups would be removed"), "{out}");
    assert_eq!(rig.packages().len(), 8, "a dry run removes nothing");
    assert_eq!(FileLedger::open(&rig.target).unwrap().records().len(), 8);
    let secret = password();
    // A short test password ("spot") is a word of every site name: only a real one is checked.
    assert!(
        secret.len() < 8 || !out.contains(&secret),
        "no secret in the output"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_deletes_exactly_the_listed_packages_and_their_ledger_entries() {
    let retention =
        "[backup.retention]\nkeep_daily = 1\nkeep_weekly = 0\nkeep_monthly = 0\nkeep_last = 5\n";
    let Some(rig) = Rig::new(retention).await else {
        return;
    };
    for n in 1..=8_u64 {
        seed(&rig.target, rig.site(), n, u32::try_from(n).unwrap());
    }
    let site = rig.site().to_owned();
    let (exit, out) = backup_cmd(&rig.env.root.0, &["prune", "--site", &site, "--apply"]).await;
    assert_eq!(exit.code(), 0, "{out}");
    let left: Vec<String> = rig
        .packages()
        .iter()
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    let want: Vec<String> = (4..=8).map(|n| format!("{site}-main-{n}")).collect();
    assert_eq!(left, want);
    let ledger = FileLedger::open(&rig.target).unwrap();
    let mut ids: Vec<String> = ledger.records().iter().map(|r| r.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, want);
    assert_eq!(ledger.last_verified().unwrap().id, format!("{site}-main-8"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_last_applies_to_unverified_backups_and_the_last_good_one_still_survives() {
    let retention =
        "[backup.retention]\nkeep_daily = 0\nkeep_weekly = 0\nkeep_monthly = 0\nkeep_last = 1\n";
    let Some(rig) = Rig::new(retention).await else {
        return;
    };
    for n in 1..=3_u64 {
        seed(&rig.target, rig.site(), n, u32::try_from(n).unwrap());
    }
    // The newest backup failed its verification: it is kept by keep_last, and backup 3 (the
    // newest verified one) is kept as the last good.
    let ledger = FileLedger::open(&rig.target).unwrap();
    let site = rig.site().to_owned();
    let newest = format!("{site}-main-3");
    let mut record = ledger
        .records()
        .into_iter()
        .find(|r| r.id == newest)
        .unwrap();
    record.verified = false;
    ledger.try_upsert(record).unwrap();
    let (exit, out) = backup_cmd(&rig.env.root.0, &["prune", "--site", &site]).await;
    assert_eq!(exit.code(), 0, "{out}");
    assert!(out.contains(&format!("keep {newest} (last)")), "{out}");
    assert!(
        out.contains(&format!("keep {site}-main-2 (last_verified)")),
        "{out}"
    );
    assert!(out.contains(&format!("remove {site}-main-1")), "{out}");
}
