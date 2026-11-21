//! The `spot-snapshot` adapter against a fake provider: a real manifest and dump file,
//! the two-pass chunk stream, the staged restore and the cleanup.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use spot_snapshot::hash::sha256_file;
use spot_snapshot::manifest::MANIFEST_NAME;
use spot_snapshot::{
    Capabilities, Consistency, Destination, ProgressSink, RestoreOptions, RestoreTarget, Snapshot,
    SnapshotError, SnapshotManifest, SnapshotOptions, SnapshotProvider, SnapshotResult,
    SnapshotTarget, VerifyReport,
};
use spot_tenancy::{ClusterId, DatabaseId, Placement, Region, TenantId};
use spot_tenant_move::{MoveMode, MovePlan, SnapshotTransfer, TransferPort};

const DUMP: &[u8] = b"the tenant's logical dump";

fn tenant() -> TenantId {
    TenantId::new_v7(1_700_000_000_000, [7; 10])
}

fn plan() -> MovePlan {
    let tenant = tenant();
    let placement = |cluster: &str, database: &str| Placement {
        cluster: ClusterId::new(cluster).unwrap(),
        database: DatabaseId::new(database).unwrap(),
        schema: tenant.schema_name(),
    };
    MovePlan::new(
        tenant,
        placement("a", "db1"),
        placement("b", "db2"),
        Region::new("eu-west").unwrap(),
        MoveMode::Offline,
    )
    .unwrap()
}

#[derive(Default)]
struct FakeProvider {
    restored: Mutex<Option<String>>,
}

impl SnapshotProvider for FakeProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_online: true,
            needs_stopped_instance: false,
            incremental: false,
            max_parallel: 1,
            estimated_throughput: 0,
        }
    }

    fn name(&self) -> &'static str {
        "fake"
    }

    fn take(
        &self,
        target: &SnapshotTarget,
        dest: &Destination,
        _opts: &SnapshotOptions,
        _progress: Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<spot_snapshot::TakeOutcome>> + Send + '_>> {
        let tenant = target.tenant().to_owned();
        let dir = dest.dir.clone();
        Box::pin(async move {
            tokio::fs::create_dir_all(&dir).await?;
            tokio::fs::write(dir.join("dump.bin"), DUMP).await?;
            let checksum = sha256_file(&dir.join("dump.bin")).await?;
            let mut files = BTreeMap::new();
            files.insert("dump.bin".to_owned(), checksum);
            let manifest = SnapshotManifest {
                id: "018bcfe5-6800-7707-8707-070707070707".to_owned(),
                tenant,
                provider: "fake".to_owned(),
                provider_version: "0.1.0".to_owned(),
                provider_kind: "fake".to_owned(),
                pg_major: spot_snapshot::SUPPORTED_MAJOR_VERSION,
                catalogue_version: "1".to_owned(),
                feed_schema: "1".to_owned(),
                outbox_generation: 1,
                outbox_last_published_seq: 0,
                started_at: "2026-01-01T00:00:00Z".to_owned(),
                finished_at: "2026-01-01T00:00:01Z".to_owned(),
                consistency: Consistency::Offline,
                total_bytes: u64::try_from(DUMP.len()).unwrap(),
                file_count: 1,
                files,
                node_local: BTreeMap::new(),
                platform_extra: BTreeMap::new(),
            };
            let text = toml::to_string(&manifest).unwrap();
            tokio::fs::write(dir.join(MANIFEST_NAME), text).await?;
            Ok(spot_snapshot::TakeOutcome {
                snapshot: Snapshot::open(dir).await?,
            })
        })
    }

    fn restore(
        &self,
        snapshot: &Snapshot,
        target: &RestoreTarget,
        _opts: &RestoreOptions,
        _progress: Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<spot_snapshot::restore::Restored>> + Send + '_>>
    {
        let location = match target {
            RestoreTarget::Database { database, .. } => database.clone(),
            RestoreTarget::Instance { .. } => "instance".to_owned(),
        };
        *self.restored.lock().unwrap() = Some(snapshot.dir().display().to_string());
        let snapshot = snapshot.clone();
        Box::pin(async move {
            Ok(spot_snapshot::restore::Restored {
                snapshot,
                location,
                activated: true,
                replaced_aside: None,
            })
        })
    }

    fn verify(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<VerifyReport>> + Send + '_>> {
        let snapshot = snapshot.clone();
        Box::pin(async move { snapshot.verify().await })
    }

    fn delete(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<()>> + Send + '_>> {
        let dir = snapshot.dir().to_path_buf();
        Box::pin(async move {
            tokio::fs::remove_dir_all(&dir)
                .await
                .map_err(|error| SnapshotError::io(format!("delete failed: {error}")))
        })
    }
}

#[tokio::test]
async fn the_adapter_streams_the_dump_imports_it_and_cleans_up() {
    let temp = tempfile::tempdir().unwrap();
    let destination = Destination {
        dir: temp.path().join("snapshot"),
    };
    let staging = temp.path().join("staging");
    let provider = Arc::new(FakeProvider::default());
    let adapter = SnapshotTransfer::new(
        provider.clone(),
        SnapshotTarget::Database {
            connection: "postgres://localhost/tenant".to_owned(),
            tenant: tenant().to_string(),
            database: "tenant".to_owned(),
        },
        destination.clone(),
        staging.clone(),
        RestoreTarget::Database {
            connection: "postgres://localhost/target".to_owned(),
            database: "target".to_owned(),
        },
    );
    let plan = plan();

    // Pass one: export every chunk.
    let mut chunks = Vec::new();
    let mut after = None;
    while let Some(chunk) = adapter.export_chunk(&plan, after).await.unwrap() {
        assert!(chunk.verify());
        after = Some(chunk.sequence);
        chunks.push(chunk);
    }
    assert!(
        chunks.len() >= 2,
        "a manifest chunk and a dump chunk at least"
    );
    assert!(chunks.last().unwrap().last);
    let bytes: u64 = chunks.iter().map(spot_tenant_move::Chunk::len).sum();
    assert!(bytes > u64::try_from(DUMP.len()).unwrap());

    // Pass two: import every chunk into the staging directory and restore.
    for chunk in &chunks {
        adapter.import_chunk(&plan, chunk).await.unwrap();
    }
    adapter.finish_import(&plan).await.unwrap();
    assert!(provider.restored.lock().unwrap().is_some());

    adapter.cleanup(&plan).await.unwrap();
    assert!(!destination.dir.exists());
    assert!(!staging.exists());
}
