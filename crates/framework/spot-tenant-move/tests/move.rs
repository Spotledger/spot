//! The move engine against fake ports: the happy path, a failure at every state with
//! abort, a corrupted chunk, a fingerprint mismatch, region refusal and idempotent resume.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use spot_rehearsal::{Dimension, Fingerprint, FingerprintRow, FingerprintSection};
use spot_tenancy::{ClusterId, DatabaseId, Placement, Region, TenantId};
use spot_tenant_move::{
    AllowedRegions, BoxFuture, Chunk, Error, FingerprintSource as MoveFingerprintSource,
    MemoryMoveStore, MoveEngine, MoveId, MovePlan, MoveRecord, MoveState, MoveStore, PlacementPort,
    QuiescePort, ResidencyPolicy, Result, Side, TransferPort, TransferTotals, Verification,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn tenant() -> TenantId {
    TenantId::new_v7(1_700_000_000_000, [7; 10])
}

fn placement(cluster: &str, database: &str) -> Placement {
    Placement {
        cluster: ClusterId::new(cluster).unwrap(),
        database: DatabaseId::new(database).unwrap(),
        schema: tenant().schema_name(),
    }
}

fn plan(region: &str) -> MovePlan {
    MovePlan::new(
        tenant(),
        placement("a", "db1"),
        placement("b", "db2"),
        Region::new(region).unwrap(),
        spot_tenant_move::MoveMode::Offline,
    )
    .unwrap()
}

fn fingerprint(count: i128) -> Fingerprint {
    Fingerprint::new(vec![FingerprintSection::new(
        "documents",
        "Documents",
        vec!["doctype".to_owned()],
        vec!["count".to_owned()],
        vec![FingerprintRow::new(
            vec![Dimension::new("doctype", "invoice")],
            BTreeMap::from([("count".to_owned(), count)]),
        )],
    )])
}

// ---------------------------------------------------------------------------
// Fake ports
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FakeTransfer {
    chunks: Vec<Chunk>,
    fail_export_at: Mutex<Option<u64>>,
    fail_import_at: Mutex<Option<u64>>,
    imports: Mutex<Vec<u64>>,
    cleanups: Mutex<u32>,
    finished: Mutex<u32>,
}

impl FakeTransfer {
    fn new(bodies: &[&[u8]]) -> Self {
        let mut offset = 0;
        let last = bodies.len().saturating_sub(1);
        let chunks = bodies
            .iter()
            .enumerate()
            .map(|(index, body)| {
                let chunk = Chunk::new(
                    u64::try_from(index).unwrap(),
                    "dump.bin",
                    offset,
                    body.to_vec(),
                    index == last,
                );
                offset += u64::try_from(body.len()).unwrap();
                chunk
            })
            .collect();
        Self {
            chunks,
            fail_export_at: Mutex::new(None),
            fail_import_at: Mutex::new(None),
            imports: Mutex::new(Vec::new()),
            cleanups: Mutex::new(0),
            finished: Mutex::new(0),
        }
    }

    fn standard() -> Self {
        Self::new(&[b"abc", b"de", b"f"])
    }

    fn fail_export_at(&self, sequence: u64) {
        *self.fail_export_at.lock().unwrap() = Some(sequence);
    }

    fn fail_import_at(&self, sequence: u64) {
        *self.fail_import_at.lock().unwrap() = Some(sequence);
    }

    fn imports(&self) -> Vec<u64> {
        self.imports.lock().unwrap().clone()
    }

    fn cleanups(&self) -> u32 {
        *self.cleanups.lock().unwrap()
    }

    fn corrupt(&mut self, sequence: usize) {
        let chunk = &mut self.chunks[sequence];
        if let Some(first) = chunk.bytes.first_mut() {
            *first ^= 0xff;
        }
    }
}

impl TransferPort for FakeTransfer {
    fn export_chunk<'a>(
        &'a self,
        _plan: &'a MovePlan,
        after: Option<u64>,
    ) -> BoxFuture<'a, Result<Option<Chunk>>> {
        Box::pin(async move {
            let sequence = after.map_or(0, |last| last.saturating_add(1));
            let mut fail = self.fail_export_at.lock().unwrap();
            if *fail == Some(sequence) {
                *fail = None;
                return Err(Error::TransferFailed {
                    tenant: "tenant".to_owned(),
                    reason: "the fake export failed",
                });
            }
            drop(fail);
            Ok(self
                .chunks
                .get(usize::try_from(sequence).unwrap_or(usize::MAX))
                .cloned())
        })
    }

    fn import_chunk<'a>(
        &'a self,
        _plan: &'a MovePlan,
        chunk: &'a Chunk,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if *self.fail_import_at.lock().unwrap() == Some(chunk.sequence) {
                *self.fail_import_at.lock().unwrap() = None;
                return Err(Error::TransferFailed {
                    tenant: "tenant".to_owned(),
                    reason: "the fake import failed",
                });
            }
            self.imports.lock().unwrap().push(chunk.sequence);
            Ok(())
        })
    }

    fn finish_import<'a>(&'a self, _plan: &'a MovePlan) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            *self.finished.lock().unwrap() += 1;
            Ok(())
        })
    }

    fn cleanup<'a>(&'a self, _plan: &'a MovePlan) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            *self.cleanups.lock().unwrap() += 1;
            Ok(())
        })
    }
}

#[derive(Default)]
struct QuiesceState {
    quiesced: bool,
    quiesce_calls: u32,
    resume_calls: u32,
    fail_next: bool,
}

#[derive(Default)]
struct FakeQuiesce {
    state: Mutex<QuiesceState>,
}

impl FakeQuiesce {
    fn fail_next(&self) {
        self.state.lock().unwrap().fail_next = true;
    }

    fn is_quiesced(&self) -> bool {
        self.state.lock().unwrap().quiesced
    }

    /// The fake closes writes and holds external effects for exactly as long as it is
    /// quiesced.
    fn writes_allowed(&self) -> bool {
        !self.state.lock().unwrap().quiesced
    }

    fn resume_calls(&self) -> u32 {
        self.state.lock().unwrap().resume_calls
    }

    fn quiesce_calls(&self) -> u32 {
        self.state.lock().unwrap().quiesce_calls
    }
}

impl QuiescePort for FakeQuiesce {
    fn quiesce<'a>(
        &'a self,
        _plan: &'a MovePlan,
        _move_id: &'a MoveId,
        _now: u64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            if state.fail_next {
                state.fail_next = false;
                return Err(Error::QuiesceFailed {
                    tenant: "tenant".to_owned(),
                    reason: "the fake gate failed",
                });
            }
            state.quiesced = true;
            state.quiesce_calls += 1;
            Ok(())
        })
    }

    fn resume<'a>(
        &'a self,
        _plan: &'a MovePlan,
        _move_id: &'a MoveId,
        _now: u64,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            state.quiesced = false;
            state.resume_calls += 1;
            Ok(())
        })
    }
}

#[derive(Default)]
struct PlacementState {
    placement: Option<Placement>,
    cutovers: u32,
    restores: u32,
    fail_next: bool,
}

#[derive(Default)]
struct FakePlacement {
    state: Mutex<PlacementState>,
}

impl FakePlacement {
    fn at(placement: Placement) -> Self {
        let fake = Self::default();
        fake.state.lock().unwrap().placement = Some(placement);
        fake
    }

    fn fail_next(&self) {
        self.state.lock().unwrap().fail_next = true;
    }

    fn placement(&self) -> Option<Placement> {
        self.state.lock().unwrap().placement.clone()
    }

    fn cutovers(&self) -> u32 {
        self.state.lock().unwrap().cutovers
    }
}

impl PlacementPort for FakePlacement {
    fn cut_over<'a>(&'a self, plan: &'a MovePlan, _now: u64) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            if state.fail_next {
                state.fail_next = false;
                return Err(Error::CutOverFailed {
                    tenant: "tenant".to_owned(),
                    reason: "the fake placement failed",
                });
            }
            state.placement = Some(plan.to.clone());
            state.cutovers += 1;
            Ok(())
        })
    }

    fn restore<'a>(&'a self, plan: &'a MovePlan) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut state = self.state.lock().unwrap();
            state.placement = Some(plan.from.clone());
            state.restores += 1;
            Ok(())
        })
    }
}

struct FakeFingerprints {
    source: Fingerprint,
    target: Fingerprint,
}

impl FakeFingerprints {
    fn matching() -> Self {
        Self {
            source: fingerprint(10),
            target: fingerprint(10),
        }
    }

    fn mismatched() -> Self {
        Self {
            source: fingerprint(10),
            target: fingerprint(11),
        }
    }
}

impl MoveFingerprintSource for FakeFingerprints {
    fn measure<'a>(
        &'a self,
        _plan: &'a MovePlan,
        side: Side,
    ) -> BoxFuture<'a, Result<Fingerprint>> {
        Box::pin(async move {
            Ok(match side {
                Side::Source => self.source.clone(),
                Side::Target => self.target.clone(),
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    plan: MovePlan,
    store: MemoryMoveStore,
    transfer: FakeTransfer,
    quiesce: FakeQuiesce,
    placement: FakePlacement,
    residency: AllowedRegions,
    fingerprints: FakeFingerprints,
}

impl Harness {
    fn new() -> Self {
        Self::with_fingerprints(FakeFingerprints::matching())
    }

    fn with_fingerprints(fingerprints: FakeFingerprints) -> Self {
        let plan = plan("eu-west");
        let mut residency = AllowedRegions::new();
        residency.allow(plan.tenant, plan.region.clone());
        Self {
            placement: FakePlacement::at(plan.from.clone()),
            plan,
            store: MemoryMoveStore::new(),
            transfer: FakeTransfer::standard(),
            quiesce: FakeQuiesce::default(),
            residency,
            fingerprints,
        }
    }

    fn engine(&self) -> MoveEngine<'_> {
        MoveEngine {
            store: &self.store,
            transfer: &self.transfer,
            quiesce: &self.quiesce,
            placement: &self.placement,
            residency: &self.residency,
            fingerprints: &self.fingerprints,
        }
    }

    fn start(&self) -> MoveRecord {
        self.engine()
            .start(self.plan.clone(), MoveId::new("move-01").unwrap(), 1)
            .unwrap()
    }

    fn tenant(&self) -> TenantId {
        self.plan.tenant
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_quiesces_exports_imports_verifies_and_cleans() {
    let harness = Harness::new();
    let engine = harness.engine();
    let record = harness.start();
    assert_eq!(record.state, MoveState::Planned);

    let record = engine.run(&harness.tenant(), 2).await.unwrap();
    assert_eq!(record.state, MoveState::Cleaned);
    assert!(harness.quiesce.writes_allowed());
    assert_eq!(
        record.exported,
        TransferTotals {
            chunks: 3,
            bytes: 6
        }
    );
    assert_eq!(record.imported, record.exported);
    assert!(record.verification.as_ref().unwrap().matched);
    assert_eq!(record.history.len(), 7);
    assert_eq!(harness.placement.placement(), Some(harness.plan.to.clone()));
    assert_eq!(harness.placement.cutovers(), 1);
    assert!(!harness.quiesce.is_quiesced());
    assert_eq!(harness.quiesce.resume_calls(), 1);
    assert_eq!(harness.transfer.imports(), vec![0, 1, 2]);
    assert_eq!(harness.transfer.cleanups(), 1);

    // A retried run after Cleaned is a no-op.
    let again = engine.run(&harness.tenant(), 3).await.unwrap();
    assert_eq!(again.state, MoveState::Cleaned);
    assert_eq!(harness.placement.cutovers(), 1);
}

#[tokio::test]
async fn abort_before_cutover_restores_the_source_at_every_state() {
    for (steps, expected) in [
        (0, MoveState::Planned),
        (1, MoveState::Quiesced),
        (2, MoveState::Exported),
        (3, MoveState::Imported),
        (4, MoveState::Verified),
    ] {
        let harness = Harness::new();
        let engine = harness.engine();
        let mut record = harness.start();
        for _ in 0..steps {
            record = engine.advance(&harness.tenant(), 2).await.unwrap();
        }
        assert_eq!(record.state, expected);
        if steps >= 1 {
            // From `Quiesced` on, no write lands and no held effect leaves the source.
            assert!(!harness.quiesce.writes_allowed());
        }
        let record = engine
            .abort(&harness.tenant(), 3, "operator cancelled")
            .await
            .unwrap();
        assert_eq!(record.state, MoveState::Aborted);
        assert!(!harness.quiesce.is_quiesced());
        assert!(harness.quiesce.writes_allowed());
        assert_eq!(
            harness.placement.placement(),
            Some(harness.plan.from.clone())
        );
        assert_eq!(harness.transfer.cleanups(), 1);
        // The source serves again and the terminal record is stable.
        assert!(matches!(
            record.halt,
            Some(spot_tenant_move::MoveHalt::Aborted { .. })
        ));
        let again = engine.run(&harness.tenant(), 4).await.unwrap();
        assert_eq!(again.state, MoveState::Aborted);
    }
}

#[tokio::test]
async fn a_failure_at_each_state_can_be_aborted_back_to_the_source() {
    // Planned: the gate refuses to quiesce.
    let harness = Harness::new();
    harness.quiesce.fail_next();
    let engine = harness.engine();
    harness.start();
    let error = engine.advance(&harness.tenant(), 2).await.unwrap_err();
    assert_eq!(error.code(), spot_tenant_move::codes::QUIESCE_FAILED);
    assert_eq!(
        engine.load(&harness.tenant()).unwrap().state,
        MoveState::Planned
    );
    engine
        .abort(&harness.tenant(), 3, "gate failed")
        .await
        .unwrap();

    // Quiesced: the export fails after the first chunk and resumes from its checkpoint.
    let harness = Harness::new();
    harness.transfer.fail_export_at(1);
    let engine = harness.engine();
    harness.start();
    engine.advance(&harness.tenant(), 2).await.unwrap();
    let error = engine.advance(&harness.tenant(), 2).await.unwrap_err();
    assert_eq!(error.code(), spot_tenant_move::codes::TRANSFER_FAILED);
    let record = engine.load(&harness.tenant()).unwrap();
    assert_eq!(record.state, MoveState::Quiesced);
    assert_eq!(record.export_next, 1);
    engine
        .abort(&harness.tenant(), 3, "export failed")
        .await
        .unwrap();

    // Exported: the import fails and leaves the first chunk imported.
    let harness = Harness::new();
    harness.transfer.fail_import_at(1);
    let engine = harness.engine();
    harness.start();
    harness.advance_until(&engine, MoveState::Exported).await;
    let error = engine.advance(&harness.tenant(), 2).await.unwrap_err();
    assert_eq!(error.code(), spot_tenant_move::codes::TRANSFER_FAILED);
    let record = engine.load(&harness.tenant()).unwrap();
    assert_eq!(record.state, MoveState::Exported);
    assert_eq!(record.import_next, 1);
    engine
        .abort(&harness.tenant(), 3, "import failed")
        .await
        .unwrap();

    // Imported: the fingerprints differ; the move halts and never cuts over.
    let harness = Harness::with_fingerprints(FakeFingerprints::mismatched());
    let engine = harness.engine();
    harness.start();
    harness.advance_until(&engine, MoveState::Imported).await;
    let error = engine.advance(&harness.tenant(), 2).await.unwrap_err();
    assert_eq!(error.code(), spot_tenant_move::codes::FINGERPRINT_MISMATCH);
    let record = engine.load(&harness.tenant()).unwrap();
    assert_eq!(record.state, MoveState::Imported);
    assert!(matches!(
        record.halt,
        Some(spot_tenant_move::MoveHalt::FingerprintMismatch { .. })
    ));
    assert_eq!(harness.placement.cutovers(), 0);
    engine
        .abort(&harness.tenant(), 3, "fingerprint mismatch")
        .await
        .unwrap();

    // Verified: the placement flip fails; abort restores the source placement.
    let harness = Harness::new();
    harness.placement.fail_next();
    let engine = harness.engine();
    harness.start();
    harness.advance_until(&engine, MoveState::Verified).await;
    let error = engine.advance(&harness.tenant(), 2).await.unwrap_err();
    assert_eq!(error.code(), spot_tenant_move::codes::CUTOVER_FAILED);
    let record = engine
        .abort(&harness.tenant(), 3, "cut-over failed")
        .await
        .unwrap();
    assert_eq!(record.state, MoveState::Aborted);
    assert_eq!(
        harness.placement.placement(),
        Some(harness.plan.from.clone())
    );
    assert!(!harness.quiesce.is_quiesced());
}

impl Harness {
    async fn advance_until(&self, engine: &MoveEngine<'_>, state: MoveState) -> MoveRecord {
        let mut record = engine.load(&self.tenant()).unwrap();
        while record.state != state {
            record = engine.advance(&self.tenant(), 2).await.unwrap();
        }
        record
    }
}

#[tokio::test]
async fn a_corrupted_chunk_is_detected_before_it_is_imported() {
    let mut harness = Harness::new();
    harness.transfer.corrupt(1);
    let engine = harness.engine();
    harness.start();
    engine.advance(&harness.tenant(), 2).await.unwrap();
    let error = engine.advance(&harness.tenant(), 2).await.unwrap_err();
    assert_eq!(error.code(), spot_tenant_move::codes::CHUNK_CORRUPT);
    let record = engine.load(&harness.tenant()).unwrap();
    assert_eq!(record.state, MoveState::Quiesced);
    assert_eq!(record.export_next, 1);
    assert!(harness.transfer.imports().is_empty());
}

#[tokio::test]
async fn a_fingerprint_mismatch_halts_before_cutover() {
    let harness = Harness::with_fingerprints(FakeFingerprints::mismatched());
    let engine = harness.engine();
    harness.start();
    harness.advance_until(&engine, MoveState::Imported).await;
    let error = engine.advance(&harness.tenant(), 2).await.unwrap_err();
    assert_eq!(error.code(), spot_tenant_move::codes::FINGERPRINT_MISMATCH);
    let record = engine.load(&harness.tenant()).unwrap();
    assert_eq!(record.state, MoveState::Imported);
    let verification = record.verification.unwrap();
    assert!(!verification.matched);
    assert_eq!(verification.differences, 1);
    assert_ne!(verification.source_digest, verification.target_digest);
    assert_eq!(harness.placement.cutovers(), 0);
    let again = harness.engine().load(&harness.tenant()).unwrap();
    assert_eq!(again.state, MoveState::Imported);
}

#[tokio::test]
async fn a_region_the_policy_does_not_allow_is_refused_before_anything_moves() {
    let harness = Harness::new();
    let mut residency = AllowedRegions::new();
    residency.allow(harness.plan.tenant, Region::new("eu-west").unwrap());
    let plan = plan("us-east");
    let engine = MoveEngine {
        residency: &residency,
        ..harness.engine()
    };
    let error = engine
        .start(plan, MoveId::new("move-02").unwrap(), 1)
        .unwrap_err();
    assert_eq!(error.code(), spot_tenant_move::codes::RESIDENCY_REFUSED);
    assert!(harness.store.get(&harness.tenant()).unwrap().is_none());
    assert_eq!(harness.quiesce.quiesce_calls(), 0);
}

#[tokio::test]
async fn a_failed_export_resumes_from_its_checkpoint() {
    let harness = Harness::new();
    harness.transfer.fail_export_at(1);
    let engine = harness.engine();
    harness.start();
    engine.advance(&harness.tenant(), 2).await.unwrap();
    assert!(engine.advance(&harness.tenant(), 2).await.is_err());
    let resumed = engine.run(&harness.tenant(), 3).await.unwrap();
    assert_eq!(resumed.state, MoveState::Cleaned);
    assert_eq!(resumed.exported.chunks, 3);
    assert_eq!(resumed.imported.chunks, 3);
    assert_eq!(harness.transfer.imports(), vec![0, 1, 2]);
}

#[tokio::test]
async fn a_failed_import_resumes_without_importing_a_chunk_twice() {
    let harness = Harness::new();
    harness.transfer.fail_import_at(1);
    let engine = harness.engine();
    harness.start();
    assert!(engine.run(&harness.tenant(), 2).await.is_err());
    let record = engine.load(&harness.tenant()).unwrap();
    assert_eq!(record.import_next, 1);
    let resumed = engine.run(&harness.tenant(), 3).await.unwrap();
    assert_eq!(resumed.state, MoveState::Cleaned);
    assert_eq!(harness.transfer.imports(), vec![0, 1, 2]);
}

#[tokio::test]
async fn abort_after_cutover_is_refused() {
    let harness = Harness::new();
    let engine = harness.engine();
    harness.start();
    harness.advance_until(&engine, MoveState::CutOver).await;
    let error = engine
        .abort(&harness.tenant(), 3, "too late")
        .await
        .unwrap_err();
    assert_eq!(error.code(), spot_tenant_move::codes::ABORT_TOO_LATE);
    assert_eq!(
        engine.load(&harness.tenant()).unwrap().state,
        MoveState::CutOver
    );
}

#[test]
fn the_store_refuses_a_stale_revision() {
    let harness = Harness::new();
    let record = harness.start();
    let stale = harness
        .store
        .save(record.clone(), record.revision + 1)
        .unwrap_err();
    assert_eq!(stale.code(), spot_tenant_move::codes::STORE_FAILED);
    let saved = harness.store.save(record.clone(), record.revision).unwrap();
    assert_eq!(saved.revision, record.revision + 1);
}

#[test]
fn a_chunk_checksum_catches_a_changed_byte() {
    let mut chunk = Chunk::new(0, "dump.bin", 0, b"hello".to_vec(), true);
    assert!(chunk.verify());
    chunk.bytes[0] ^= 0xff;
    assert!(!chunk.verify());
}

#[test]
fn a_verification_reports_the_differing_measure() {
    let verification = Verification::between(&fingerprint(10), &fingerprint(11));
    assert!(!verification.matched);
    assert_eq!(verification.differences, 1);
    assert_eq!(verification.sections, 1);
    let same = Verification::between(&fingerprint(10), &fingerprint(10));
    assert!(same.matched);
}

#[test]
fn the_residency_allow_list_is_per_tenant_and_region() {
    let harness = Harness::new();
    assert!(harness.residency.check(&harness.plan).is_ok());
    let other = plan("us-east");
    assert_eq!(
        harness.residency.check(&other).unwrap_err().code(),
        spot_tenant_move::codes::RESIDENCY_REFUSED
    );
    // The allow-list is not global: another tenant gets nothing.
    let mut allowed = AllowedRegions::new();
    allowed.allow(
        TenantId::new_v7(1_700_000_000_001, [8; 10]),
        Region::new("eu-west").unwrap(),
    );
    assert!(allowed.check(&harness.plan).is_err());
}
