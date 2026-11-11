//! Provider-level tests of the stopped-instance provider against the fake PostgreSQL
//! bundle (`tests/support/fake_pg.rs`): the stop-copy-restart round trip, the restart
//! after a failing copy, and the cancelled take that leaves only a `.partial` staging
//! neighbour which `Snapshot::open` refuses. These run on every machine — no bundled
//! PostgreSQL binaries needed; the README documents the test split.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use spot_pg_supervisor::layout::{Listen, PgLayout};
use spot_pg_supervisor::settings::PgSettings;
use spot_pg_supervisor::supervisor::{Supervisor, SupervisorState};
use spot_snapshot::provider::{
    Destination, Progress, ProgressSink, RestoreOptions, RestoreTarget, SnapshotOptions,
    SnapshotProvider, SnapshotTarget,
};
use spot_snapshot::{Snapshot, StoppedInstanceProvider};

use support::{free_port, marker_files, stage_fake_binaries, supervisor_config, wait_port};

const READY: Duration = Duration::from_secs(10);

/// A progress sink that counts the phases (the crate never prints by itself).
#[derive(Default)]
struct Phases {
    started: std::sync::atomic::AtomicUsize,
    finished_ok: std::sync::atomic::AtomicUsize,
}

impl ProgressSink for Phases {
    fn on(&self, event: Progress) {
        match event {
            Progress::Started { .. } => {
                self.started
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Progress::Finished { ok } => {
                assert!(ok, "no test drive reaches a failing finish");
                self.finished_ok
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Progress::FileDone { .. } => {}
        }
    }
}

/// One staged fake environment: a bin dir with the three roles, an initialised data
/// dir with deterministic marker files, and a free port in its `.env`.
struct FakeEnv {
    _tmp: tempfile::TempDir,
    layout: PgLayout,
    supervisor: Arc<Supervisor>,
}

impl FakeEnv {
    /// Builds the layout, the supervisor, and a stopped-but-initialised data dir whose
    /// marker files hold deterministic content. `PG_VERSION` marks it initialised, so
    /// the supervisor never runs `initdb` (the take's major-version gate needs it).
    fn new(branches: usize) -> FakeEnv {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_dir = tmp.path().join("bin");
        let data_dir = tmp.path().join("data");
        let log_dir = tmp.path().join("logs");
        std::fs::create_dir_all(&bin_dir).unwrap();
        stage_fake_binaries(&bin_dir);
        std::fs::create_dir_all(&data_dir).unwrap();
        for (name, contents) in [
            ("PG_VERSION", "18\n"),
            ("spot.conf", "port = 5433\n"),
            ("postgresql.conf", "# fake\n"),
        ] {
            std::fs::write(data_dir.join(name), contents).unwrap();
        }
        marker_files(&data_dir, branches);
        let port = free_port();
        std::fs::write(data_dir.join(".env"), format!("FAKE_PORT={port}\n")).unwrap();
        let layout = PgLayout {
            bin_dir,
            data_dir,
            log_dir,
            listen: Listen::Localhost,
            port,
        };
        let supervisor = Arc::new(Supervisor::new(
            layout.clone(),
            PgSettings::default(),
            supervisor_config(),
        ));
        FakeEnv {
            _tmp: tmp,
            layout,
            supervisor,
        }
    }
}

/// Starts and stops the fake once, leaving the stopped instance behind (the caller
/// contract `take_stopped` builds on: `start()` refuses while a supervision round is
/// live, so the provider's own stop must begin from a fresh `Stopped` supervisor).
async fn start_and_stop_once(env: &FakeEnv) {
    let handle = env.supervisor.start().await.unwrap();
    assert!(
        wait_port(env.layout.port, READY).await,
        "the fake never listened"
    );
    handle.stop(Duration::from_secs(2)).await.unwrap();
    assert!(!port_open(env.layout.port).await, "the port was freed");
}

/// Waits until the instance is serving again: the port answers a TCP connect (the
/// instance's own readiness signal) while the supervision round is not Failed.
/// Bounded; a take's or cancel's restart is observed, never assumed.
async fn wait_instance_serving(supervisor: &Supervisor, port: u16) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let failed = matches!(supervisor.state(), SupervisorState::Failed { .. });
        if !failed && port_open(port).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// One probe of the port.
async fn port_open(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

/// The staging neighbour of `dir` (the `.partial` rule's other half).
fn staging_of(dir: &Path) -> PathBuf {
    let mut name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str(".partial");
    dir.parent().unwrap_or(Path::new(".")).join(name)
}

fn provider() -> StoppedInstanceProvider {
    StoppedInstanceProvider::new(Arc::new(spot_core::SystemClock::new()))
}

fn instance_target(layout: &PgLayout, tenant: &str) -> SnapshotTarget {
    SnapshotTarget::Instance {
        layout: layout.clone(),
        tenant: tenant.to_owned(),
        password: "not-used-by-the-fake".to_owned(),
    }
}

/// Reads one branch's rows file from a data dir.
fn branch_bytes(layout: &PgLayout, branch: usize) -> Vec<u8> {
    std::fs::read(
        layout
            .data_dir
            .join(format!("branch-{branch:03}"))
            .join("rows.bin"),
    )
    .unwrap()
}

// The deterministic fixture layout of a fake data dir: PG_VERSION, spot.conf,
// postgresql.conf, .env plus `branch-NNN/rows.bin` per branch. Runtime-written
// files (the postmaster.pid the supervisor cleans and recreates, the fake's argv
// records) may exist or not at copy time, so assertions use the deterministic
// subset only.

fn sha_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The happy path: stop, checksummed copy of the whole data dir, restart, manifest
/// last; then a branch-by-branch content-equal restore into a second stopped
/// instance's populated data dir (with the explicit `replace` consent), whose
/// restarted fake serves the restored bytes.
#[tokio::test]
async fn take_copies_the_data_dir_and_restore_matches_branch_by_branch() {
    let source = FakeEnv::new(4);
    start_and_stop_once(&source).await;

    let tmp = tempfile::TempDir::new().unwrap();
    let dest = Destination {
        dir: tmp.path().join("snap"),
    };
    let phases = Arc::new(Phases::default());
    let outcome = provider()
        .take(
            &instance_target(&source.layout, "tenant-a"),
            &dest,
            &SnapshotOptions::default(),
            phases.clone(),
        )
        .await
        .unwrap();

    // The instance came back up after the copy (the guard's core promise) — observed
    // by the port answering again, never assumed.
    assert!(
        wait_instance_serving(&source.supervisor, source.layout.port).await,
        "the instance restarted after the take"
    );
    let manifest = outcome.snapshot.manifest();
    // The staging tree is the data dir's image: the manifest's keys are relative to
    // the snapshot root (what verify and restore join against). The deterministic
    // fixture subset must all be present.
    for name in [
        "PG_VERSION",
        "postgresql.conf",
        ".env",
        "branch-000/rows.bin",
        "branch-003/rows.bin",
    ] {
        assert!(
            manifest.files.contains_key(name),
            "missing {name} in {:?}",
            manifest.files.keys()
        );
    }
    assert_eq!(manifest.file_count, manifest.files.len() as u64);
    assert_eq!(manifest.consistency, spot_snapshot::Consistency::Offline);
    assert_eq!(manifest.provider_kind, "stopped");
    assert_eq!(manifest.tenant, "tenant-a");
    assert!(phases.started.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    assert_eq!(
        phases.finished_ok.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(!staging_of(&dest.dir).exists(), "staging neighbour renamed");

    // Restore into a second stopped instance's populated data dir; a replace of a
    // populated target needs the exact confirmation token (its data-dir path).
    let target_env = FakeEnv::new(2);
    start_and_stop_once(&target_env).await;
    let token = target_env.layout.data_dir.display().to_string();
    let restored = provider()
        .restore(
            &outcome.snapshot,
            &RestoreTarget::Instance {
                layout: target_env.layout.clone(),
                password: "not-used-by-the-fake".to_owned(),
            },
            &RestoreOptions {
                parallel: 4,
                replace: true,
                replace_token: token.clone(),
                deadline: Duration::ZERO,
            },
            Arc::new(Phases::default()),
        )
        .await
        .unwrap();

    // Content identical, branch by branch (the brief's table-by-table hash).
    for branch in 0..4 {
        let before = branch_bytes(&source.layout, branch);
        let after = branch_bytes(&target_env.layout, branch);
        assert_eq!(sha_hex(&before), sha_hex(&after), "branch-{branch:03}");
    }
    assert_eq!(
        restored.location,
        target_env.layout.data_dir.display().to_string()
    );
    assert!(!restored.activated, "the restore returns non-activated");
    // The moved-aside copy of the replaced data dir is kept, clearly named, never dropped.
    let aside = restored
        .replaced_aside
        .expect("a replace records the moved-aside copy");
    assert!(
        Path::new(&aside).exists(),
        "the moved-aside copy stays: {aside}"
    );

    // The restored instance starts (its stale lock from the old generation is a dead
    // pid: cleaned at start) and serves the restored bytes on the *target's* port.
    let handle = target_env.supervisor.start().await.unwrap();
    handle.ready().await.unwrap();
    assert!(port_open(target_env.layout.port).await);
    handle.stop(Duration::from_secs(2)).await.unwrap();
}

/// The taken snapshot verifies; a flipped snapshot byte fails verify naming the file.
#[tokio::test]
async fn taken_snapshot_verifies_and_names_a_flipped_byte() {
    let env = FakeEnv::new(2);
    start_and_stop_once(&env).await;

    let tmp = tempfile::TempDir::new().unwrap();
    let dest = Destination {
        dir: tmp.path().join("snap"),
    };
    let outcome = provider()
        .take(
            &instance_target(&env.layout, "tenant-a"),
            &dest,
            &SnapshotOptions::default(),
            Arc::new(Phases::default()),
        )
        .await
        .unwrap();
    let manifest = outcome.snapshot.manifest();
    let report = provider().verify(&outcome.snapshot).await.unwrap();
    assert_eq!(report.files_checked, manifest.file_count);
    assert!(report.files_checked >= 4 + 2, "{report:?}");

    let file = dest.dir.join("branch-001").join("rows.bin");
    std::fs::write(&file, "branch 001 rowz\n").unwrap();
    let error = provider().verify(&outcome.snapshot).await.unwrap_err();
    assert_eq!(error.code(), "snapshot.verify_failed");
    assert!(error.to_string().contains("branch-001"), "{error}");
}

/// The failing-copy path: a symlink inside the data dir makes the verified copy refuse
/// (symlinks are never followed out of a snapshot). The guard must STILL restart the
/// instance, and the failed take must leave no snapshot `Snapshot::open` accepts.
#[cfg(unix)]
#[tokio::test]
async fn a_failing_copy_still_restarts_the_instance() {
    let env = FakeEnv::new(3);
    start_and_stop_once(&env).await;

    let link_target = env.layout.data_dir.join("branch-000").join("rows.bin");
    std::os::unix::fs::symlink(
        &link_target,
        env.layout.data_dir.join("branch-002").join("link.bin"),
    )
    .unwrap();

    let tmp = tempfile::TempDir::new().unwrap();
    let dest = Destination {
        dir: tmp.path().join("snap"),
    };
    let error = provider()
        .take(
            &instance_target(&env.layout, "tenant-a"),
            &dest,
            &SnapshotOptions::default(),
            Arc::new(Phases::default()),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("symlink"), "{error}");

    // The restart promise: the instance listens again after the failed take (the
    // guard's recovery path; the supervisor's state is not Failed either).
    let mut restarted = false;
    for _ in 0..80 {
        if port_open(env.layout.port).await {
            restarted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(restarted, "the instance restarted after a failed copy");
    assert!(
        !matches!(env.supervisor.state(), SupervisorState::Failed { .. }),
        "the failed take must not fail the supervision round"
    );
    // The failed take leaves no snapshot and no staged tree: the destination stays
    // clean (the staging neighbour is removed on the failure path; only a *cancel*
    // mid-await leaves one, and the cancel test below proves `open` refuses it).
    assert!(!staging_of(&dest.dir).exists());
    assert!(Snapshot::open(&dest.dir).await.is_err());
}

/// Cancelling the take at an await point (dropping the future mid-copy) exercises the
/// guard's `Drop` restart on its own thread: the instance comes back, only the
/// `.partial` dir remains, and `Snapshot::open` refuses it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_the_take_leaves_a_partial_and_restarts() {
    // Many small files keep the copy busy long enough that aborting mid-copy is
    // deterministic (a tiny tree can complete between polls).
    let env = FakeEnv::new(400);
    start_and_stop_once(&env).await;

    let tmp = tempfile::TempDir::new().unwrap();
    let dest = Destination {
        dir: tmp.path().join("snap"),
    };
    let target = instance_target(&env.layout, "tenant-a");
    let test_provider = provider();
    // Boxing gives the test ownership of the future, so dropping it aborts the take
    // (a pinned stack future would keep running to its own end — a drop of the pin
    // drops nothing).
    let mut take = Box::pin(test_provider.take(
        &target,
        &dest,
        &SnapshotOptions::default(),
        Arc::new(Phases::default()),
    ));
    // Abort while the copy is still underway; the staged neighbour proves the copy
    // had begun (otherwise the cancel would prove nothing about the guard's Drop).
    let mut cancelled = false;
    for _ in 0..250 {
        match tokio::time::timeout(Duration::from_millis(2), take.as_mut()).await {
            Ok(Ok(_)) => panic!("the take finished before it could be cancelled"),
            Ok(Err(error)) => panic!("the take failed before cancellation: {error}"),
            Err(_elapsed) => {
                if staging_of(&dest.dir).exists() {
                    drop(take); // the abort path: the future's Drop runs the guard
                    cancelled = true;
                    break;
                }
            }
        }
    }
    assert!(
        cancelled,
        "the take ran to the end instead of being cancelled"
    );

    // The guard's Drop restart ran on its own thread with its own runtime.
    assert!(
        wait_instance_serving(&env.supervisor, env.layout.port).await,
        "the instance restarted after the cancelled take"
    );
    // Only the `.partial` staging neighbour remains, and it is refused.
    let staged = staging_of(&dest.dir);
    assert!(
        staged.exists(),
        "the staging neighbour stays after a cancel"
    );
    let error = Snapshot::open(&staged).await.unwrap_err();
    assert!(error.to_string().contains("staged"), "{error}");
    let error = Snapshot::open(&dest.dir).await.unwrap_err();
    assert!(error.to_string().contains("no readable"), "{error}");
}

/// The manifest's recorded bytes equal the files on disk, branch by branch (the take's
/// inventory is the truth a restore copies from).
#[tokio::test]
async fn manifest_bytes_equal_the_copied_files() {
    let env = FakeEnv::new(3);
    start_and_stop_once(&env).await;

    let tmp = tempfile::TempDir::new().unwrap();
    let dest = Destination {
        dir: tmp.path().join("snap"),
    };
    let outcome = provider()
        .take(
            &instance_target(&env.layout, "tenant-b"),
            &dest,
            &SnapshotOptions::default(),
            Arc::new(Phases::default()),
        )
        .await
        .unwrap();
    let manifest = outcome.snapshot.manifest();
    assert_eq!(manifest.tenant, "tenant-b");
    let mut sum = 0_u64;
    for (rel, recorded) in &manifest.files {
        let bytes = std::fs::read(dest.dir.join(rel)).unwrap();
        assert_eq!(u64::try_from(bytes.len()).unwrap(), recorded.bytes, "{rel}");
        assert_eq!(sha_hex(&bytes), recorded.sha256, "{rel}");
        sum = sum.saturating_add(recorded.bytes);
    }
    assert_eq!(sum, manifest.total_bytes);
    assert!(manifest.files.contains_key("branch-001/rows.bin"));
}

/// The fake records its argv while initialising; no password-looking token may appear
/// in the recorded argv or captured args (the brief's secrets rule, proven on
/// records). The supervisor drives initdb with a protected pwfile, so the argv holds
/// the file's path only — never the password itself.
#[test]
fn no_password_reaches_the_fake_initdb_arguments() {
    let env = FakeEnv::new(0);
    let password = "correct horse battery staple";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        env.supervisor.ensure_initialised(password).await.unwrap();
    });
    for name in ["fake-initdb-argv.txt", "fake-postgres-argv.txt"] {
        let path = env.layout.data_dir.join(name);
        let Ok(argv) = std::fs::read_to_string(&path) else {
            continue; // the argv file is written only on that role's own run
        };
        assert!(
            !argv.contains(password),
            "the password reached a command line in {name}: {argv}"
        );
        for segment in argv.split('\u{1}') {
            assert_ne!(segment.trim(), password, "an argv segment was the password");
        }
    }
}
