//! The snapshot adapter implements the `spot-snapshot` provider contract: a btrfs snapshot
//! is taken, verified and deleted through the trait, with the platform handle recorded in
//! the manifest; a database target is refused.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_agent_linux_sys::provider::{LinuxSnapshotProvider, PLATFORM_BACKEND, PLATFORM_SNAPSHOT};
use spot_agent_linux_sys::storage::SnapshotBackend;
use spot_agent_linux_sys::testing::{FakeFs, FakeRunner, ok_output};
use spot_core::{Clock, ManualClock};
use spot_pg_supervisor::layout::PgLayout;
use spot_snapshot::{
    Destination, Progress, ProgressSink, SnapshotOptions, SnapshotProvider, SnapshotTarget,
};

struct NoProgress;

impl ProgressSink for NoProgress {
    fn on(&self, _event: Progress) {}
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH))
}

#[tokio::test]
async fn a_btrfs_provider_takes_verifies_and_deletes_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let dest = dir.path().join("snap1");
    let source = data.display().to_string();
    let handle = dest.display().to_string();

    let runner = FakeRunner::new()
        .with_response(
            &format!("btrfs subvolume snapshot {source} {handle}"),
            ok_output(""),
        )
        .with_response(
            &format!("btrfs subvolume list -o {source}"),
            ok_output(&format!("ID 257 gen 30 top level 5 path {handle}\n")),
        )
        .with_response(&format!("btrfs subvolume delete {handle}"), ok_output(""));
    let fs = FakeFs::new();
    let provider = LinuxSnapshotProvider::new(SnapshotBackend::Btrfs, &runner, &fs, clock());
    assert_eq!(provider.provider_kind(), "btrfs");

    let target = SnapshotTarget::Instance {
        layout: PgLayout {
            data_dir: data.clone(),
            ..PgLayout::default()
        },
        tenant: "tenant-1".to_owned(),
        password: String::new(),
    };
    let destination = Destination { dir: dest.clone() };
    let progress: Arc<dyn ProgressSink> = Arc::new(NoProgress);

    let outcome = provider
        .take(
            &target,
            &destination,
            &SnapshotOptions::default(),
            Arc::clone(&progress),
        )
        .await
        .unwrap();
    let manifest = outcome.snapshot.manifest();
    assert_eq!(manifest.tenant, "tenant-1");
    assert_eq!(manifest.provider_kind, "btrfs");
    assert_eq!(
        manifest
            .platform_extra
            .get(PLATFORM_BACKEND)
            .map(String::as_str),
        Some("btrfs")
    );
    assert_eq!(
        manifest
            .platform_extra
            .get(PLATFORM_SNAPSHOT)
            .map(String::as_str),
        Some(handle.as_str())
    );

    let report = provider.verify(&outcome.snapshot).await.unwrap();
    assert_eq!(report.files_checked, 0);

    provider.delete(&outcome.snapshot).await.unwrap();
    assert!(
        runner
            .call_keys()
            .iter()
            .any(|key| key == &format!("btrfs subvolume delete {handle}"))
    );
}

#[tokio::test]
async fn a_database_target_is_refused() {
    let runner = FakeRunner::new();
    let fs = FakeFs::new();
    let provider = LinuxSnapshotProvider::new(SnapshotBackend::Btrfs, &runner, &fs, clock());
    let target = SnapshotTarget::Database {
        connection: "postgres://localhost/x".to_owned(),
        tenant: "tenant-1".to_owned(),
        database: "x".to_owned(),
    };
    let destination = Destination {
        dir: std::path::PathBuf::from("/tmp/spot-unused"),
    };
    let error = provider
        .take(
            &target,
            &destination,
            &SnapshotOptions::default(),
            Arc::new(NoProgress),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent_linux_sys.snapshot_target")
    );
}
