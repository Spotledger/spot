//! Service-side tests: entitlement, quota, immutable chunks, listing, retention and
//! audited deletion.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use spot_cloud_backup::crypto::DataKey;
use spot_cloud_backup::{
    Artefact, BackupManifest, BackupService, BackupSession, BackupState, BackupSummary, ChunkEntry,
    FixedSource, InMemoryBackupService, ManifestKey, MemoryObjectStore, RecordingAudit,
    RecordingStore, RecoveryKey, RestoreCheckpoint, RestoreOptions, RetentionPolicy, SourceInfo,
    StorageQuota, TenantAuth, UploadOptions, UploadRequest, VerifyMode, chunk, crypto,
    execute_retention, plan_retention, restore,
};
use spot_core::SiteId;
use spot_license::model::{Edition, Kind, RawLicence};
use spot_license::{Entitlements, Licence};

const NOW: i64 = 1_700_000_000;
const CANARY: &[u8] = b"SPOT-CANARY-PLAINTEXT-9c1f-do-not-upload";

fn tenant() -> SiteId {
    SiteId::new("acme.example").expect("site id")
}

fn other_tenant() -> SiteId {
    SiteId::new("other.example").expect("site id")
}

fn licence(holder: &str, cloud_backup: bool) -> Licence {
    Licence::new(RawLicence {
        v: 1,
        licence_id: format!("lic-{holder}"),
        tenant: Some(holder.to_owned()),
        installation_id: None,
        edition: Edition::new("business").expect("edition"),
        ent: Entitlements::from_lists(
            None,
            Vec::new(),
            if cloud_backup {
                vec!["cloud_backup".to_owned()]
            } else {
                Vec::new()
            },
        ),
        kind: Kind::Subscription,
        issued_at: NOW - 1_000,
        not_before: NOW - 1_000,
        expires_at: Some(NOW + 365 * 86_400),
        renew_after: None,
        grace: 86_400,
        serial: 1,
        key_id: "issuer-1".to_owned(),
    })
    .expect("licence")
}

fn tenant_auth(tenant: SiteId, cloud_backup: bool) -> TenantAuth {
    TenantAuth::new(tenant.clone(), licence(tenant.as_str(), cloud_backup), NOW).expect("auth")
}

fn artefact() -> Artefact {
    let mut bytes = CANARY.to_vec();
    for step in 0..300u16 {
        bytes.push(u8::try_from(step % 251).expect("byte"));
    }
    Artefact::new("site.tar", bytes)
}

fn options() -> UploadOptions {
    UploadOptions {
        chunk_size: 64,
        verify: VerifyMode::Full,
        retries: 1,
        ..UploadOptions::default()
    }
}

async fn run(
    service: &dyn BackupService,
    auth: &TenantAuth,
    recovery: &RecoveryKey,
    source: &FixedSource,
    now: i64,
) -> spot_cloud_backup::BackupOutcome {
    let mut session = BackupSession::with_service(
        auth.tenant().clone(),
        recovery.clone(),
        source,
        service,
        auth.clone(),
        options(),
    );
    session.run(now).await.expect("backup")
}

fn summaries(listed: &[spot_cloud_backup::StoredBackup]) -> Vec<BackupSummary> {
    listed
        .iter()
        .map(|stored| BackupSummary {
            backup_id: stored.backup_id.clone(),
            created_at: stored.created_at,
            state: stored.state.clone(),
            bytes: stored.cipher_bytes,
        })
        .collect()
}

#[tokio::test]
async fn a_service_backup_is_listed_verified_and_restorable() {
    let store = Arc::new(RecordingStore::new());
    let audit = Arc::new(RecordingAudit::new());
    let service =
        InMemoryBackupService::new(store.clone(), StorageQuota::unlimited(), audit.clone());
    let auth = tenant_auth(tenant(), true);
    let recovery = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(artefact());

    let outcome = run(&service, &auth, &recovery, &source, NOW).await;
    assert_eq!(outcome.manifest.state, BackupState::Verified);

    let listed = service.list(&auth).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].backup_id, outcome.manifest.backup_id);
    assert_eq!(listed[0].state, BackupState::Verified);
    assert_eq!(listed[0].manifest_version, 2);
    let manifest = service
        .manifest(&auth, &outcome.manifest.backup_id)
        .await
        .expect("manifest");
    assert_eq!(manifest.version, 2);
    assert_eq!(
        service.used_bytes(&auth).await.expect("used"),
        outcome.uploaded_bytes
    );

    // Only ciphertext crossed the service's store.
    for (key, bytes) in store.puts() {
        assert!(
            !bytes.windows(CANARY.len()).any(|window| window == CANARY),
            "plaintext canary reached {key}"
        );
    }

    // The customer restores from the service's objects and the recovery key alone.
    let plain = artefact().bytes;
    let mut checkpoint = RestoreCheckpoint::new(outcome.manifest.backup_id.clone());
    let restored = restore(
        store.as_ref(),
        &tenant(),
        &outcome.manifest.backup_id,
        &recovery,
        RestoreOptions::default(),
        &mut checkpoint,
    )
    .await
    .expect("restore");
    assert_eq!(restored.bytes, plain);
}

#[tokio::test]
async fn a_tenant_over_quota_is_refused_before_anything_is_written() {
    let store = Arc::new(RecordingStore::new());
    let service = InMemoryBackupService::new(
        store.clone(),
        StorageQuota::from_bytes(100),
        Arc::new(RecordingAudit::new()),
    );
    let auth = tenant_auth(tenant(), true);
    let recovery = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(artefact());
    let mut session = BackupSession::with_service(
        tenant(),
        recovery,
        &source,
        &service,
        auth.clone(),
        options(),
    );
    let error = session.run(NOW).await.expect_err("quota");
    assert_eq!(error.code(), "cloud_backup.quota_exceeded");
    assert!(store.puts().is_empty(), "nothing was written");
    assert_eq!(service.used_bytes(&auth).await.expect("used"), 0);
}

#[tokio::test]
async fn a_licence_without_the_feature_or_for_another_tenant_is_refused() {
    let missing =
        TenantAuth::new(tenant(), licence("acme.example", false), NOW).expect_err("feature");
    assert_eq!(missing.code(), "cloud_backup.not_entitled");
    let wrong_tenant =
        TenantAuth::new(tenant(), licence("other.example", true), NOW).expect_err("tenant");
    assert_eq!(wrong_tenant.code(), "cloud_backup.not_entitled");
}

#[tokio::test]
async fn chunks_are_immutable_and_a_commit_needs_every_one() {
    let store = Arc::new(MemoryObjectStore::new());
    let service = InMemoryBackupService::new(
        store,
        StorageQuota::unlimited(),
        Arc::new(RecordingAudit::new()),
    );
    let auth = tenant_auth(tenant(), true);
    let key = DataKey::generate().expect("rng");
    let plain = b"0123456789";
    let sealed = chunk::seal("bk-immutable", &key, plain, 4).expect("seal");
    let cipher_bytes: u64 = sealed.iter().map(|chunk| chunk.cipher_len()).sum();
    let request = UploadRequest {
        backup_id: "bk-immutable".to_owned(),
        chunk_count: u32::try_from(sealed.len()).expect("n"),
        cipher_bytes,
    };
    let ticket = service.begin(&auth, &request, NOW).await.expect("begin");
    let object = BackupManifest::chunk_object("acme.example", "bk-immutable", 0, 0);
    let receipt = service
        .put_chunk(&auth, &ticket, &sealed[0], &object)
        .await
        .expect("put");
    assert!(receipt.stored);
    let again = service
        .put_chunk(&auth, &ticket, &sealed[0], &object)
        .await
        .expect("idempotent retry");
    assert!(!again.stored);

    // Different bytes under the same immutable key are a conflict, not an overwrite.
    let mut changed = sealed[0].clone();
    changed.bytes[0] ^= 0x01;
    changed.digest = crypto::sha256(&changed.bytes);
    let conflict = service
        .put_chunk(&auth, &ticket, &changed, &object)
        .await
        .expect_err("conflict");
    assert_eq!(conflict.code(), "cloud_backup.object_conflict");

    // A commit before every chunk is uploaded is refused.
    let entry = ChunkEntry {
        index: 0,
        offset: 0,
        plain_len: u64::try_from(plain.len()).expect("n"),
        cipher_len: sealed[0].cipher_len(),
        sha256: sealed[0].digest_hex(),
        object,
    };
    let manifest = BackupManifest::new(
        "bk-immutable",
        "acme.example",
        NOW,
        SourceInfo {
            name: "site.tar".to_owned(),
            bytes: u64::try_from(plain.len()).expect("n"),
            sha256: crypto::sha256_hex(plain),
        },
        4,
        vec![entry],
        vec![ManifestKey {
            label: "recovery".to_owned(),
            key_id: "tenant-recovery".to_owned(),
            key_version: 1,
            wrapped: "AAAA".to_owned(),
        }],
    );
    let refused = service
        .commit(&auth, &ticket, &manifest)
        .await
        .expect_err("missing chunks");
    assert_eq!(refused.code(), "cloud_backup.state");
}

#[tokio::test]
async fn retention_deletes_only_what_it_selects_and_audits_every_deletion() {
    let store = Arc::new(RecordingStore::new());
    let audit = Arc::new(RecordingAudit::new());
    let service = InMemoryBackupService::new(store, StorageQuota::unlimited(), audit.clone());
    let auth = tenant_auth(tenant(), true);
    let recovery = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(artefact());
    let first = run(&service, &auth, &recovery, &source, NOW).await;
    let second = run(&service, &auth, &recovery, &source, NOW + 1).await;
    let before = service.used_bytes(&auth).await.expect("used");
    assert!(before > 0);

    let policy = RetentionPolicy::new(1, 0, 0).expect("policy");
    let listed = service.list(&auth).await.expect("list");
    assert_eq!(listed.len(), 2);
    let plan = plan_retention(&tenant(), &policy, &summaries(&listed), NOW + 2, &[]).expect("plan");
    assert_eq!(plan.delete.len(), 1);
    assert_eq!(plan.delete[0], first.manifest.backup_id);
    assert!(!plan.delete.contains(&second.manifest.backup_id));

    // Another tenant cannot use this plan's permit.
    let other_auth = tenant_auth(other_tenant(), true);
    let refused = service
        .delete(&other_auth, &plan.permits()[0], "intruder", NOW + 2)
        .await
        .expect_err("cross tenant");
    assert_eq!(refused.code(), "cloud_backup.delete_refused");
    assert!(audit.events().is_empty());

    let deleted = execute_retention(&service, &auth, &plan, "ops@example", NOW + 2)
        .await
        .expect("retention");
    assert_eq!(deleted, 1);
    let events = audit.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].backup_id, first.manifest.backup_id);
    assert_eq!(events[0].plan, plan.plan_id);
    assert_eq!(events[0].actor, "ops@example");
    assert!(events[0].at == NOW + 2);

    let after = service.list(&auth).await.expect("list");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].backup_id, second.manifest.backup_id);
    assert!(service.used_bytes(&auth).await.expect("used") < before);
}
