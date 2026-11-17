//! Helpers over the verifier, the planner and the scheduled audit.

use spot_journal_audit::Position;
use spot_journal_audit_run::memory::{MemoryLive, MemoryScratch, SteppingClock};
use spot_journal_audit_run::{AuditOptions, AuditRun, run_audit};
use spot_journal_manifest::{SignedManifest, Verified, verify};
use spot_journal_rebuild::RebuildKeys;
use spot_journal_rebuild_plan::{ForkInfo, JournalCatalog, SegmentRange};
use spot_journal_snapshot::{SnapshotError, SnapshotInfo, SnapshotParams, SnapshotWriter};
use spot_journal_store::{JournalStore, Layout, TenantId};
use spot_journal_verify::PinnedExpected;

use crate::events::{Model, SOURCE, TENANT};
use crate::keys::{manifest_keys, snapshot_keys, snapshot_signer};
use crate::store::TestStore;

pub fn pin(generation: u64) -> PinnedExpected {
    PinnedExpected::new(TenantId::new(TENANT).unwrap(), SOURCE, generation).unwrap()
}

pub async fn stored_manifest(store: &TestStore, generation: u64) -> Verified {
    let key = Layout::manifest(TENANT, generation).unwrap().to_key();
    let object = store.get(&key).await.unwrap().unwrap();
    verify(
        &SignedManifest::from_bytes(&object.bytes).unwrap(),
        &manifest_keys(),
    )
    .unwrap()
}

/// The catalogue the planner reads: every given manifest's segments, the newest one's forks.
pub fn catalog(manifests: &[&Verified], snapshots: Vec<SnapshotInfo>) -> JournalCatalog {
    let mut segments = Vec::new();
    for manifest in manifests {
        segments.extend(manifest.segments().iter().map(|s| SegmentRange {
            generation: s.generation,
            first_seq: s.first_seq,
            last_seq: s.last_seq,
        }));
    }
    let forks = manifests
        .iter()
        .max_by_key(|m| m.generation())
        .map(|m| {
            m.forks()
                .iter()
                .map(|f| ForkInfo {
                    generation: f.generation,
                    from_position: f.forked_from_position,
                })
                .collect()
        })
        .unwrap_or_default();
    JournalCatalog {
        snapshots,
        segments,
        forks,
    }
}

/// Writes a snapshot of `model` as of `(generation, position)`; it is not yet marked verified.
pub async fn write_snapshot(
    store: &TestStore,
    model: &Model,
    generation: u64,
    position: u64,
) -> SnapshotInfo {
    try_write_snapshot(store, model, generation, position)
        .await
        .unwrap()
}

/// As [`write_snapshot`], returning the writer's error (a crash test injects one).
pub async fn try_write_snapshot(
    store: &TestStore,
    model: &Model,
    generation: u64,
    position: u64,
) -> Result<SnapshotInfo, SnapshotError> {
    let params = SnapshotParams {
        tenant: TENANT.to_owned(),
        generation,
        position,
        manifest_hash: "ab".repeat(32),
        taken_at_millis: 1_700_000_000_000,
    };
    let signer = snapshot_signer();
    let mut writer =
        SnapshotWriter::new(store, params, &signer).and_then(|w| w.with_part_bytes(400))?;
    writer.write_part(model.records()).await?;
    writer.finish().await
}

/// The live side holding exactly `model`, with its head at `(generation, head)`.
pub fn live_of(model: &Model, generation: u64, head: u64) -> MemoryLive {
    let mut live = MemoryLive::new(TENANT);
    for ((entity, id), (version, body, position)) in &model.0 {
        live.put(*position, entity, id, *version, body);
    }
    live.set_head(Position::new(generation, head));
    live
}

/// Runs the scheduled audit of generation 1 against `live` over `ranges`.
pub async fn audit(store: &TestStore, live: &MemoryLive, ranges: Vec<SegmentRange>) -> AuditRun {
    let manifest = manifest_keys();
    let snapshots = snapshot_keys();
    let keys = RebuildKeys {
        manifest: &manifest,
        snapshot: &snapshots,
    };
    let options = AuditOptions::new(TenantId::new(TENANT).unwrap(), SOURCE, 1, ranges, 60_000);
    let mut scratch = MemoryScratch::new(TENANT);
    run_audit(
        store,
        &keys,
        &spot_image_upcast::Registry::new(),
        live,
        &mut scratch,
        &options,
        &SteppingClock::new(10_000, 5),
    )
    .await
    .unwrap()
}
