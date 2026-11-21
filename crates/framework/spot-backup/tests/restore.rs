//! Restore workflow tests: the preflight matrix, forward migrations, fail-closed
//! behaviour, one-click restore on an empty fake site, and the L7 registry proving
//! invariants hold after the restore.
#![allow(missing_docs)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use spot_backup::package::PackageEntry;
use spot_backup::site::{InvariantRunner, RestoredSite};
use spot_backup::testing::{FakeRestoreSite, FakeServiceGate, sample_manifest};
use spot_backup::{
    InvariantReport, RestoreOptions, RestorePorts, RestoreStatus, ServiceGate, one_click_restore,
    preflight, restore,
};
use spot_l7_invariants::builtin::framework_registry;
use spot_l7_invariants::history::History;
use spot_l7_invariants::invariant::{CheckContext, RunMeta};
use spot_l7_invariants::model::Model;
use spot_l7_invariants::oracle::Oracle;
use spot_storage_contract::StorageBackend;
use spot_storage_fake::FakeBackend;

fn work_package(
    schema_version: u64,
    app_set: Vec<&str>,
    tenant: &str,
) -> spot_backup::PackagedBackup {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "spot-backup-tests-{}-{sequence}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("work dir");
    let mut manifest = sample_manifest("acme-chain-1", tenant, schema_version);
    manifest.app_set = app_set.into_iter().map(str::to_owned).collect();
    manifest.app_set.sort();
    spot_backup::package::build_from_files(
        manifest,
        vec![PackageEntry {
            path: "snapshot.toml".to_owned(),
            bytes: b"tenant = \"acme\"\n".to_vec(),
        }],
        &dir,
    )
    .expect("build")
}

fn site() -> FakeRestoreSite {
    FakeRestoreSite::new("acme", 3, 18, vec!["crm".to_owned()], 1 << 30, true)
}

#[test]
fn preflight_refuses_the_whole_matrix() {
    let built = work_package(3, vec!["crm"], "acme");

    // Clean target.
    assert!(
        preflight(
            built.package.manifest(),
            &site(),
            &RestoreOptions::default()
        )
        .ok
    );

    // Newer backup schema is refused unless explicitly allowed.
    let newer = work_package(5, vec!["crm"], "acme");
    let refused = preflight(
        newer.package.manifest(),
        &site(),
        &RestoreOptions::default(),
    );
    assert!(!refused.ok);
    assert_eq!(refused.blockers[0].code, "backup.newer_schema");
    assert!(
        preflight(
            newer.package.manifest(),
            &site(),
            &RestoreOptions {
                allow_newer_schema: true,
                replace: false,
            }
        )
        .ok
    );

    // Missing app.
    let missing = work_package(3, vec!["crm", "erp"], "acme");
    let refused = preflight(
        missing.package.manifest(),
        &site(),
        &RestoreOptions::default(),
    );
    assert!(!refused.ok);
    assert!(refused.reason().contains("erp"));

    // Not enough free space.
    let small = FakeRestoreSite::new("acme", 3, 18, vec!["crm".to_owned()], 1, true);
    assert!(!preflight(built.package.manifest(), &small, &RestoreOptions::default()).ok);

    // Populated target without replace.
    let populated = FakeRestoreSite::new("acme", 3, 18, vec!["crm".to_owned()], 1 << 30, false);
    assert!(
        !preflight(
            built.package.manifest(),
            &populated,
            &RestoreOptions::default()
        )
        .ok
    );
    assert!(
        preflight(
            built.package.manifest(),
            &populated,
            &RestoreOptions {
                allow_newer_schema: false,
                replace: true,
            }
        )
        .ok
    );

    // Wrong tenant.
    let wrong = FakeRestoreSite::new("other", 3, 18, vec!["crm".to_owned()], 1 << 30, true);
    assert!(!preflight(built.package.manifest(), &wrong, &RestoreOptions::default()).ok);
}

#[tokio::test]
async fn a_restore_on_an_empty_site_closes_then_reopens_the_gate() {
    let built = work_package(3, vec!["crm"], "acme");
    let site = site();
    let gate = FakeServiceGate::open();
    let invariants = spot_backup::testing::FakeInvariantRunner::passing();
    let ports = RestorePorts {
        service: &gate,
        invariants: &invariants,
    };
    let report = restore(
        built.package.manifest(),
        &built.package,
        &site,
        &ports,
        &RestoreOptions::default(),
    )
    .await;
    assert!(report.restored(), "{report:?}");
    assert!(report.service_open);
    assert_eq!(gate.closes(), 1);
    assert_eq!(gate.opens(), 1);
    assert!(site.applied());
    assert!(site.reset());
}

#[tokio::test]
async fn a_newer_schema_refusal_never_touches_the_service() {
    let built = work_package(5, vec!["crm"], "acme");
    let site = site();
    let gate = FakeServiceGate::open();
    let invariants = spot_backup::testing::FakeInvariantRunner::passing();
    let ports = RestorePorts {
        service: &gate,
        invariants: &invariants,
    };
    let report = one_click_restore(
        built.package.manifest(),
        &built.package,
        &site,
        &ports,
        &RestoreOptions::default(),
    )
    .await;
    assert!(matches!(report.status, RestoreStatus::Refused));
    assert_eq!(gate.closes(), 0);
    assert!(!site.applied());
    assert!(gate.is_open());
}

#[tokio::test]
async fn an_older_backup_migrates_forward_before_the_invariants() {
    let built = work_package(2, vec!["crm"], "acme");
    let site = site();
    let gate = FakeServiceGate::open();
    let invariants = spot_backup::testing::FakeInvariantRunner::passing();
    let ports = RestorePorts {
        service: &gate,
        invariants: &invariants,
    };
    let report = restore(
        built.package.manifest(),
        &built.package,
        &site,
        &ports,
        &RestoreOptions::default(),
    )
    .await;
    assert!(report.restored(), "{report:?}");
    assert_eq!(site.migrated(), vec![(2, 3)]);
    match report.status {
        RestoreStatus::Restored { migration, .. } => {
            assert_eq!(migration.expect("migration").to, 3);
        }
        other => panic!("expected restored, got {other:?}"),
    }
}

#[tokio::test]
async fn failed_invariants_leave_the_service_closed() {
    let built = work_package(3, vec!["crm"], "acme");
    let site = site();
    let gate = FakeServiceGate::open();
    let invariants = spot_backup::testing::FakeInvariantRunner::failing(vec![
        "outbox-complete: missing".to_owned(),
    ]);
    let ports = RestorePorts {
        service: &gate,
        invariants: &invariants,
    };
    let report = restore(
        built.package.manifest(),
        &built.package,
        &site,
        &ports,
        &RestoreOptions::default(),
    )
    .await;
    assert!(!report.restored());
    assert!(!report.service_open);
    assert!(!gate.is_open());
    assert_eq!(gate.opens(), 0);
    match report.status {
        RestoreStatus::Failed { stage, .. } => assert_eq!(stage, "invariants"),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_package_that_does_not_match_the_manifest_is_refused() {
    let built = work_package(3, vec!["crm"], "acme");
    let other = sample_manifest("other-chain-9", "acme", 3);
    let site = site();
    let gate = FakeServiceGate::open();
    let invariants = spot_backup::testing::FakeInvariantRunner::passing();
    let ports = RestorePorts {
        service: &gate,
        invariants: &invariants,
    };
    let report = restore(
        &other,
        &built.package,
        &site,
        &ports,
        &RestoreOptions::default(),
    )
    .await;
    assert!(matches!(report.status, RestoreStatus::Refused));
    assert_eq!(gate.closes(), 0);
    assert!(!site.applied());
}

/// The acceptance path: restore into an empty fake site, then the real L7 registry
/// (over the in-memory storage backend) runs every framework invariant and passes.
struct L7Runner {
    backend: Arc<FakeBackend>,
    model: Arc<Model>,
}

impl InvariantRunner for L7Runner {
    fn run<'a>(
        &'a self,
        site: &'a dyn RestoredSite,
    ) -> spot_backup::BoxFuture<'a, spot_backup::Result<InvariantReport>> {
        Box::pin(async move {
            let _restored = site
                .as_any()
                .downcast_ref::<FakeRestoreSite>()
                .ok_or_else(|| spot_backup::BackupError::io("the restored site is not the fake"))?;
            let history = Arc::new(History::new(7, 1, "backup-restore"));
            let oracle = Arc::new(Oracle::new());
            let context = CheckContext::for_backend(
                self.backend.clone(),
                self.model.catalog.clone(),
                history,
                oracle,
            );
            let registry = framework_registry(&self.model);
            let report = registry
                .check_all(
                    &context,
                    RunMeta {
                        seed: 7,
                        backend: "fake".to_owned(),
                        profile: "backup-restore".to_owned(),
                        workers: 1,
                    },
                )
                .await;
            if report.ok {
                Ok(InvariantReport::passed("l7", registry.len() as u64))
            } else {
                Ok(InvariantReport::failed(
                    "l7",
                    registry.len() as u64,
                    report
                        .results
                        .iter()
                        .filter(|result| !result.passed)
                        .map(|result| result.id.clone())
                        .collect(),
                ))
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restore_on_an_empty_fake_site_passes_the_l7_invariants() {
    let built = work_package(3, vec!["crm"], "acme");
    let model = Arc::new(Model::new().expect("model"));
    let backend = Arc::new(FakeBackend::new());
    backend.prepare(&model.catalog).await.expect("prepare");
    let site = site();
    let gate = FakeServiceGate::open();
    let runner = L7Runner {
        backend: backend.clone(),
        model,
    };
    let ports = RestorePorts {
        service: &gate,
        invariants: &runner,
    };
    let report = one_click_restore(
        built.package.manifest(),
        &built.package,
        &site,
        &ports,
        &RestoreOptions::default(),
    )
    .await;
    assert!(report.restored(), "{report:?}");
    assert!(gate.is_open());
    match report.status {
        RestoreStatus::Restored { invariants, .. } => {
            assert!(invariants.ok, "{invariants:?}");
            assert!(invariants.checks_run >= 9, "{invariants:?}");
        }
        other => panic!("expected a restored report, got {other:?}"),
    }
}

#[tokio::test]
async fn a_tampered_package_is_refused_before_the_service_closes() {
    let built = work_package(3, vec!["crm"], "acme");
    let mut bytes = std::fs::read(built.package.path()).expect("read");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(built.package.path(), bytes).expect("write");
    let site = site();
    let gate = FakeServiceGate::open();
    let invariants = spot_backup::testing::FakeInvariantRunner::passing();
    let ports = RestorePorts {
        service: &gate,
        invariants: &invariants,
    };
    let report = restore(
        built.package.manifest(),
        &built.package,
        &site,
        &ports,
        &RestoreOptions::default(),
    )
    .await;
    assert!(
        matches!(report.status, RestoreStatus::Refused),
        "{report:?}"
    );
    assert_eq!(gate.closes(), 0);
    assert!(!site.applied());
}
