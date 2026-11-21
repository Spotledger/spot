//! End-to-end client tests: upload, verification, repair, restore and resume.
//!
//! The canary test proves the promise the service model rests on: nothing but ciphertext
//! and wrapped keys crosses the object store.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Mutex;

use spot_cloud_backup::{
    Artefact, BackupSession, BackupState, Escrow, FixedSource, MemoryObjectStore, ObjectInfo,
    ObjectStore, PutOutcome, RecordingStore, RecoveryKey, RestoreCheckpoint, RestoreOptions,
    StoreFuture, UploadOptions, VerifyMode, restore, restore_with_key,
};
use spot_core::SiteId;

const NOW: i64 = 1_700_000_000;
const CANARY: &[u8] = b"SPOT-CANARY-PLAINTEXT-9c1f-do-not-upload";

fn tenant() -> SiteId {
    SiteId::new("acme.example").expect("site id")
}

fn artefact() -> Artefact {
    let mut bytes = CANARY.to_vec();
    for step in 0..400u16 {
        bytes.push(u8::try_from(step % 251).expect("byte"));
    }
    Artefact::new("site.tar", bytes)
}

fn options(chunk_size: u64, verify: VerifyMode) -> UploadOptions {
    UploadOptions {
        chunk_size,
        verify,
        retries: 1,
        ..UploadOptions::default()
    }
}

/// A store that flips one byte in the first read of the first chunk of generation 0.
struct CorruptingStore {
    inner: MemoryObjectStore,
    armed: Mutex<bool>,
}

impl CorruptingStore {
    fn new() -> Self {
        Self {
            inner: MemoryObjectStore::new(),
            armed: Mutex::new(true),
        }
    }
}

impl ObjectStore for CorruptingStore {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, PutOutcome> {
        self.inner.put(key, bytes)
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let mut bytes = self.inner.get(key).await?;
            if key.contains("chunk-00000000-g0000") {
                let mut armed = self.armed.lock().expect("lock");
                if *armed {
                    *armed = false;
                    let last = bytes.len() - 1;
                    bytes[last] ^= 0x01;
                }
            }
            Ok(bytes)
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        self.inner.list(prefix)
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.inner.delete(key)
    }
}

/// A store that silently drops the first write of chunk 1 of generation 0.
struct DroppingStore {
    inner: MemoryObjectStore,
    armed: Mutex<bool>,
}

impl DroppingStore {
    fn new() -> Self {
        Self {
            inner: MemoryObjectStore::new(),
            armed: Mutex::new(true),
        }
    }
}

impl ObjectStore for DroppingStore {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, PutOutcome> {
        Box::pin(async move {
            if key.contains("chunk-00000001-g0000") {
                let mut armed = self.armed.lock().expect("lock");
                if *armed {
                    *armed = false;
                    return Ok(PutOutcome::Stored);
                }
            }
            self.inner.put(key, bytes).await
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>> {
        self.inner.get(key)
    }

    fn list<'a>(&'a self, prefix: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        self.inner.list(prefix)
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.inner.delete(key)
    }
}

/// A store that fails the first read of one chunk, like a dropped connection.
struct FlakyReadStore {
    inner: MemoryObjectStore,
    fragment: String,
    armed: Mutex<bool>,
}

impl FlakyReadStore {
    fn new(inner: MemoryObjectStore, fragment: &str) -> Self {
        Self {
            inner,
            fragment: fragment.to_owned(),
            armed: Mutex::new(true),
        }
    }
}

impl ObjectStore for FlakyReadStore {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, PutOutcome> {
        self.inner.put(key, bytes)
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>> {
        Box::pin(async move {
            if key.contains(&self.fragment) {
                let mut armed = self.armed.lock().expect("lock");
                if *armed {
                    *armed = false;
                    return Err(spot_cloud_backup::CloudBackupError::Store {
                        reason: "the connection dropped".to_owned(),
                    });
                }
            }
            self.inner.get(key).await
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        self.inner.list(prefix)
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.inner.delete(key)
    }
}

#[tokio::test]
async fn a_backup_uploads_only_ciphertext_and_restores_on_a_new_pc() {
    let tenant = tenant();
    let recovery = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(artefact());
    let store = RecordingStore::new();
    let mut session = BackupSession::new(
        tenant.clone(),
        recovery.clone(),
        &source,
        &store,
        options(64, VerifyMode::Full),
    );
    let outcome = session.run(NOW).await.expect("backup");
    assert_eq!(outcome.manifest.state, BackupState::Verified);
    assert_eq!(outcome.manifest.version, 2);
    assert!(outcome.verification.failures.is_empty());
    let plain = artefact().bytes;
    assert_eq!(
        u64::try_from(outcome.manifest.chunks.len()).expect("n"),
        u64::try_from(plain.len()).expect("n").div_ceil(64)
    );

    // Nothing but ciphertext and wrapped keys crossed the store.
    for (key, bytes) in store.puts() {
        assert!(
            !bytes.windows(CANARY.len()).any(|window| window == CANARY),
            "plaintext canary reached {key}"
        );
        assert!(
            !bytes.windows(32).any(|window| window == &plain[..32]),
            "plaintext prefix reached {key}"
        );
        assert_ne!(bytes, plain);
    }
    assert_eq!(store.written_bytes(), outcome.uploaded_bytes);

    // The phrase is the recovery key; a new PC needs the phrase and the store only.
    let phrase = outcome.recovery_phrase.clone();
    let manifest = outcome.manifest.clone();
    assert_eq!(phrase.key().expose(), recovery.expose());

    let mut checkpoint = RestoreCheckpoint::new(manifest.backup_id.clone());
    let restored = restore(
        &store,
        &tenant,
        &manifest.backup_id,
        &phrase.key(),
        RestoreOptions::default(),
        &mut checkpoint,
    )
    .await
    .expect("restore");
    assert_eq!(restored.bytes, plain);
    assert_eq!(restored.name, "site.tar");
    assert_eq!(restored.chunks_skipped, 0);
}

#[tokio::test]
async fn a_tampered_chunk_is_detected_and_restore_refuses_it() {
    let tenant = tenant();
    let recovery = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(artefact());
    let store = MemoryObjectStore::new();
    let mut session = BackupSession::new(
        tenant.clone(),
        recovery.clone(),
        &source,
        &store,
        options(64, VerifyMode::Full),
    );
    let outcome = session.run(NOW).await.expect("backup");
    assert_eq!(outcome.manifest.state, BackupState::Verified);

    let key = outcome.manifest.chunks[1].object.clone();
    let mut bytes = store.get(&key).await.expect("get");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    store.delete(&key).await.expect("delete");
    store.put(&key, bytes).await.expect("put");

    let mut checkpoint = RestoreCheckpoint::new(outcome.manifest.backup_id.clone());
    let error = restore(
        &store,
        &tenant,
        &outcome.manifest.backup_id,
        &recovery,
        RestoreOptions::default(),
        &mut checkpoint,
    )
    .await
    .expect_err("corrupt");
    assert_eq!(error.code(), "cloud_backup.chunk_corrupt");
    let message = error.to_string();
    assert!(
        !message
            .as_bytes()
            .windows(CANARY.len())
            .any(|window| window == CANARY),
        "the error must not echo plaintext"
    );
}

#[tokio::test]
async fn a_corrupt_read_is_repaired_as_a_new_generation() {
    let tenant = tenant();
    let recovery = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(artefact());
    let store = CorruptingStore::new();
    let mut session = BackupSession::new(
        tenant.clone(),
        recovery.clone(),
        &source,
        &store,
        options(64, VerifyMode::Full),
    );
    let outcome = session.run(NOW).await.expect("backup");
    assert_eq!(outcome.manifest.state, BackupState::Verified);
    assert_eq!(outcome.retries, 1);
    assert!(
        outcome.manifest.chunks[0].object.contains("-g0001.bin"),
        "the repaired chunk is a new generation: {}",
        outcome.manifest.chunks[0].object
    );
    // Both generations stay in the immutable store; the newest manifest names g0001.
    assert!(
        store
            .list("acme.example/")
            .await
            .expect("list")
            .iter()
            .any(|object| object.key.contains("chunk-00000000-g0000.bin"))
    );
    let mut checkpoint = RestoreCheckpoint::new(outcome.manifest.backup_id.clone());
    let restored = restore(
        &store,
        &tenant,
        &outcome.manifest.backup_id,
        &recovery,
        RestoreOptions::default(),
        &mut checkpoint,
    )
    .await
    .expect("restore");
    assert_eq!(restored.bytes, artefact().bytes);
}

#[tokio::test]
async fn a_dropped_chunk_upload_is_retried_and_verified() {
    let tenant = tenant();
    let recovery = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(artefact());
    let store = DroppingStore::new();
    let mut session = BackupSession::new(
        tenant.clone(),
        recovery.clone(),
        &source,
        &store,
        options(64, VerifyMode::Full),
    );
    let outcome = session.run(NOW).await.expect("backup");
    assert_eq!(outcome.manifest.state, BackupState::Verified);
    assert_eq!(outcome.retries, 1);
    assert!(outcome.manifest.chunks[1].object.contains("-g0001.bin"));
}

#[tokio::test]
async fn a_dropped_connection_resumes_at_the_next_chunk() {
    let tenant = tenant();
    let recovery = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(artefact());
    let store = MemoryObjectStore::new();
    let mut session = BackupSession::new(
        tenant.clone(),
        recovery.clone(),
        &source,
        &store,
        options(64, VerifyMode::Full),
    );
    let outcome = session.run(NOW).await.expect("backup");
    let manifest = outcome.manifest.clone();
    drop(session);

    let flaky = FlakyReadStore::new(store, "chunk-00000002");
    let mut checkpoint = RestoreCheckpoint::new(manifest.backup_id.clone());
    let error = restore(
        &flaky,
        &tenant,
        &manifest.backup_id,
        &recovery,
        RestoreOptions::default(),
        &mut checkpoint,
    )
    .await
    .expect_err("dropped");
    assert_eq!(error.code(), "cloud_backup.restore");
    assert_eq!(checkpoint.completed(), &[0, 1]);
    assert_eq!(checkpoint.partial_len(), 128);

    let restored = restore(
        &flaky,
        &tenant,
        &manifest.backup_id,
        &recovery,
        RestoreOptions::default(),
        &mut checkpoint,
    )
    .await
    .expect("resume");
    assert_eq!(restored.bytes, artefact().bytes);
    assert_eq!(restored.chunks_skipped, 2);
    assert_eq!(
        restored.chunks_downloaded,
        u64::try_from(manifest.chunks.len()).expect("n") - 2
    );
}

#[tokio::test]
async fn a_wrong_recovery_key_fails_before_any_chunk_is_read() {
    let tenant = tenant();
    let recovery = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(artefact());
    let store = RecordingStore::new();
    let mut session = BackupSession::new(
        tenant.clone(),
        recovery.clone(),
        &source,
        &store,
        options(64, VerifyMode::Full),
    );
    let outcome = session.run(NOW).await.expect("backup");
    store.clear();

    let wrong = RecoveryKey::generate().expect("rng");
    let mut checkpoint = RestoreCheckpoint::new(outcome.manifest.backup_id.clone());
    let error = restore(
        &store,
        &tenant,
        &outcome.manifest.backup_id,
        &wrong,
        RestoreOptions::default(),
        &mut checkpoint,
    )
    .await
    .expect_err("wrong key");
    assert_eq!(error.code(), "cloud_backup.wrong_key");
    assert!(
        !store.gets().iter().any(|key| key.contains("chunk-")),
        "a wrong key must not read a chunk"
    );
    let message = error.to_string();
    assert!(!message.contains(&wrong.phrase().as_str()[..10]));
    assert!(!message.contains(&outcome.recovery_phrase.as_str()[..10]));
}

#[tokio::test]
async fn escrow_opens_the_backup_when_the_recovery_key_is_lost() {
    let tenant = tenant();
    let recovery = RecoveryKey::generate().expect("rng");
    let escrow = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(artefact());
    let store = MemoryObjectStore::new();
    let mut options = options(64, VerifyMode::Full);
    options.escrow = Escrow::granted(std::sync::Arc::new(escrow.provider()), "company-key-2026")
        .expect("escrow");
    let mut session =
        BackupSession::new(tenant.clone(), recovery.clone(), &source, &store, options);
    let outcome = session.run(NOW).await.expect("backup");
    assert!(
        outcome
            .manifest
            .key_with_label("company-key-2026")
            .is_some()
    );

    let mut checkpoint = RestoreCheckpoint::new(outcome.manifest.backup_id.clone());
    let restored = restore_with_key(
        &store,
        &tenant,
        &outcome.manifest.backup_id,
        &escrow.provider(),
        "company-key-2026",
        RestoreOptions::default(),
        &mut checkpoint,
    )
    .await
    .expect("escrow restore");
    assert_eq!(restored.bytes, artefact().bytes);

    // The recovery key still works too.
    let mut checkpoint = RestoreCheckpoint::new(outcome.manifest.backup_id.clone());
    restore(
        &store,
        &tenant,
        &outcome.manifest.backup_id,
        &recovery,
        RestoreOptions::default(),
        &mut checkpoint,
    )
    .await
    .expect("recovery restore");
}

#[tokio::test]
async fn an_empty_artefact_is_one_empty_chunk_and_round_trips() {
    let tenant = tenant();
    let recovery = RecoveryKey::generate().expect("rng");
    let source = FixedSource::new(Artefact::new("empty.tar", Vec::new()));
    let store = MemoryObjectStore::new();
    let mut session = BackupSession::new(
        tenant.clone(),
        recovery.clone(),
        &source,
        &store,
        options(64, VerifyMode::Full),
    );
    let outcome = session.run(NOW).await.expect("backup");
    assert_eq!(outcome.manifest.chunks.len(), 1);
    assert_eq!(outcome.manifest.chunks[0].plain_len, 0);
    let mut checkpoint = RestoreCheckpoint::new(outcome.manifest.backup_id.clone());
    let restored = restore(
        &store,
        &tenant,
        &outcome.manifest.backup_id,
        &recovery,
        RestoreOptions::default(),
        &mut checkpoint,
    )
    .await
    .expect("restore");
    assert!(restored.bytes.is_empty());
}
