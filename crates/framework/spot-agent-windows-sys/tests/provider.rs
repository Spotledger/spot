//! The `spot-snapshot` provider adapter on the in-memory fakes: a take runs the coordinated
//! sequence, writes a canonical manifest, and deletes the shadow copy; restore copies the
//! verified files back into an instance's data directory.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use spot_agent_windows_sys::{
    BackupHorizon, FakePgBackup, FakeVss, FixedFacts, SnapshotFacts, VssProvider,
};
use spot_pg_supervisor::layout::{Listen, PgLayout};
use spot_snapshot::provider::{
    Destination, Progress, ProgressSink, RestoreOptions, RestoreTarget, SnapshotOptions,
    SnapshotProvider, SnapshotTarget,
};
use time::OffsetDateTime;

/// A progress sink that discards every event.
struct NullProgress;

impl ProgressSink for NullProgress {
    fn on(&self, _event: Progress) {}
}

struct Fixture {
    provider: VssProvider,
    vss: Arc<FakeVss>,
    backup: Arc<FakePgBackup>,
    layout: PgLayout,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("data");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("PG_VERSION"), b"18\n").unwrap();
    std::fs::write(source.join("base.tar"), b"pretend base backup").unwrap();

    // The shadow root mirrors the volume: the fake exposes `shadow/data`.
    let shadow_root = dir.path().join("shadow");
    let shadow_data = shadow_root.join("data");
    std::fs::create_dir_all(&shadow_data).unwrap();
    std::fs::write(shadow_data.join("PG_VERSION"), b"18\n").unwrap();
    std::fs::write(shadow_data.join("base.tar"), b"pretend base backup").unwrap();

    let vss = Arc::new(FakeVss::new(
        spot_agent_windows_sys::VolumeId::new("C:\\").unwrap(),
        &shadow_root,
    ));
    let backup = Arc::new(FakePgBackup::new(BackupHorizon { xmin: 42, epoch: 1 }));
    let facts = FixedFacts(SnapshotFacts {
        catalogue_version: "2026-01-01".to_owned(),
        feed_schema: "1".to_owned(),
        outbox_generation: 3,
        outbox_last_published_seq: 99,
        xmin: 42,
        epoch: 1,
    });
    let clock = Arc::new(spot_core::ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let provider = VssProvider::new(vss.clone(), backup.clone(), Arc::new(facts), clock);
    let layout = PgLayout {
        bin_dir: dir.path().join("bin"),
        data_dir: source,
        log_dir: dir.path().join("log"),
        listen: Listen::Localhost,
        port: 5433,
    };
    Fixture {
        provider,
        vss,
        backup,
        layout,
        _dir: dir,
    }
}

fn instance_target(layout: &PgLayout) -> SnapshotTarget {
    SnapshotTarget::Instance {
        layout: layout.clone(),
        tenant: "tenant-1".to_owned(),
        password: "not-a-real-secret".to_owned(),
    }
}

#[tokio::test]
async fn a_take_writes_a_manifest_and_cleans_up_the_shadow() {
    let fixture = fixture();
    let dest = Destination {
        dir: fixture._dir.path().join("snapshot"),
    };
    let outcome = fixture
        .provider
        .take(
            &instance_target(&fixture.layout),
            &dest,
            &SnapshotOptions::default(),
            Arc::new(NullProgress),
        )
        .await
        .unwrap();

    let manifest = outcome.snapshot.manifest();
    assert_eq!(manifest.provider_kind, "vss");
    assert_eq!(
        manifest.consistency,
        spot_snapshot::Consistency::OnlineConsistent { xmin: 42, epoch: 1 }
    );
    assert_eq!(manifest.catalogue_version, "2026-01-01");
    assert_eq!(manifest.files.len(), 2);
    assert_eq!(
        manifest.platform_extra.get("volumes").map(String::as_str),
        Some("C:\\")
    );
    assert!(!fixture.backup.is_active(), "backup mode must be off");
    assert_eq!(
        fixture.vss.count(),
        0,
        "the shadow is deleted after the copy"
    );

    let report = fixture.provider.verify(&outcome.snapshot).await.unwrap();
    assert_eq!(report.files_checked, 2);

    fixture.provider.delete(&outcome.snapshot).await.unwrap();
    assert!(!dest.dir.exists());
}

#[tokio::test]
async fn a_take_refuses_a_database_target() {
    let fixture = fixture();
    let dest = Destination {
        dir: fixture._dir.path().join("snapshot"),
    };
    let target = SnapshotTarget::Database {
        connection: "postgres://user@host:5432/db".to_owned(),
        tenant: "tenant-1".to_owned(),
        database: "db".to_owned(),
    };
    let error = fixture
        .provider
        .take(
            &target,
            &dest,
            &SnapshotOptions::default(),
            Arc::new(NullProgress),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "snapshot.io");
    assert_eq!(fixture.backup.starts(), 0);
}

#[tokio::test]
async fn a_restore_copies_the_verified_files_back() {
    let fixture = fixture();
    let dest = Destination {
        dir: fixture._dir.path().join("snapshot"),
    };
    let outcome = fixture
        .provider
        .take(
            &instance_target(&fixture.layout),
            &dest,
            &SnapshotOptions::default(),
            Arc::new(NullProgress),
        )
        .await
        .unwrap();

    // A fresh, empty data directory receives the restore.
    let restored_dir: PathBuf = fixture._dir.path().join("restored");
    let target = PgLayout {
        data_dir: restored_dir.clone(),
        ..fixture.layout.clone()
    };
    let restored = fixture
        .provider
        .restore(
            &outcome.snapshot,
            &RestoreTarget::Instance {
                layout: target,
                password: "not-a-real-secret".to_owned(),
            },
            &RestoreOptions::default(),
            Arc::new(NullProgress),
        )
        .await
        .unwrap();
    assert!(restored.activated);
    assert_eq!(
        std::fs::read(restored_dir.join("PG_VERSION")).unwrap(),
        b"18\n"
    );
}
