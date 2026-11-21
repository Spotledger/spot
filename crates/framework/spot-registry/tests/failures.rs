#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::*;
use spot_registry::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
struct FailingIndex {
    inner: MemoryIndexStore,
    fail: AtomicBool,
    tamper: AtomicBool,
}
impl Default for FailingIndex {
    fn default() -> Self {
        Self {
            inner: MemoryIndexStore::default(),
            fail: AtomicBool::new(true),
            tamper: AtomicBool::new(false),
        }
    }
}
impl IndexStore for FailingIndex {
    fn commit(&self, r: Release, p: Publisher) -> Result<PublishOutcome> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(RegistryError::new(
                "registry.storage",
                "simulated crash before index commit",
            ));
        }
        self.inner.commit(r, p)
    }
    fn release(&self, a: &AppName, v: &Version) -> Result<Option<Release>> {
        let mut r = self.inner.release(a, v)?;
        if self.tamper.load(Ordering::SeqCst)
            && let Some(r) = &mut r
        {
            r.requires_host = Some(VersionReq::parse("^99").unwrap());
        }
        Ok(r)
    }
    fn by_digest(&self, d: Digest) -> Result<Option<Release>> {
        let mut r = self.inner.by_digest(d)?;
        if self.tamper.load(Ordering::SeqCst)
            && let Some(r) = &mut r
        {
            r.publisher = "forged".into();
        }
        Ok(r)
    }
    fn preflight(&self, r: &Release) -> Result<()> {
        self.inner.preflight(r)
    }
    fn ownership(&self, a: &AppName) -> Result<Option<Ownership>> {
        self.inner.ownership(a)
    }
    fn publisher(&self, id: &str) -> Result<Option<Publisher>> {
        self.inner.publisher(id)
    }
    fn set_publisher_status(&self, id: &str, s: PublisherStatus) -> Result<()> {
        self.inner.set_publisher_status(id, s)
    }
    fn releases(&self, a: &AppName, v: &Visibility, c: Option<&Caller>) -> Result<Vec<Release>> {
        self.inner.releases(a, v, c)
    }
    fn search(&self, q: &SearchQuery) -> Result<SearchPage> {
        self.inner.search(q)
    }
    fn remove(
        &self,
        a: &AppName,
        v: &Version,
        b: &str,
        r: Removal,
        w: bool,
    ) -> Result<Option<Release>> {
        self.inner.remove(a, v, b, r, w)
    }
    fn transfer(&self, t: TransferRecord, p: Publisher) -> Result<()> {
        self.inner.transfer(t, p)
    }
    fn transfers(&self, a: &AppName) -> Result<Vec<TransferRecord>> {
        self.inner.transfers(a)
    }
    fn digests(&self) -> Result<Vec<Digest>> {
        self.inner.digests()
    }
    fn describe(&self, a: &AppName, b: &str, d: String, c: String) -> Result<()> {
        self.inner.describe(a, b, d, c)
    }
}
#[test]
fn torn_publish_reports_orphan_without_false_claim_or_audit_then_retries() {
    let index = Arc::new(FailingIndex::default());
    let blobs = Arc::new(MemoryBlobStore::default());
    let audit = Arc::new(MemoryAuditSink::default());
    let registry = Registry::new(
        index.clone(),
        blobs.clone(),
        RegistryPolicy::default(),
        trust(),
        audit.clone(),
    );
    let b = bytes("demo", "1.0.0", "");
    let digest = open(&b).digest();
    assert_code(publish(&registry, b.clone(), 12), "registry.storage");
    assert!(run(blobs.exists(digest)).unwrap());
    assert!(index.ownership(&app("demo")).unwrap().is_none());
    assert!(index.digests().unwrap().is_empty());
    assert!(audit.records().unwrap().is_empty());
    assert_eq!(run(registry.repair_orphans()).unwrap(), vec![digest]);
    index.fail.store(false, Ordering::SeqCst);
    publish(&registry, b, 12).unwrap();
    assert!(run(registry.repair_orphans()).unwrap().is_empty());
    assert_eq!(audit.records().unwrap().len(), 1);
}
#[test]
fn fetched_identity_constraints_and_certificate_publisher_not_trusted_from_index() {
    let index = Arc::new(FailingIndex::default());
    index.fail.store(false, Ordering::SeqCst);
    let registry = Registry::new(
        index.clone(),
        Arc::new(MemoryBlobStore::default()),
        RegistryPolicy::default(),
        trust(),
        Arc::new(MemoryAuditSink::default()),
    );
    let r = publish(&registry, bytes("demo", "1.0.0", ""), 12).unwrap();
    index.tamper.store(true, Ordering::SeqCst);
    assert_code(
        run(registry.fetch(&r.app, &r.version, NOW)),
        "registry.metadata_mismatch",
    );
    assert_code(
        run(registry.fetch_digest(r.digest, NOW)),
        "registry.metadata_mismatch",
    );
}
#[derive(Default)]
struct HookBlob {
    inner: MemoryBlobStore,
    fail_put: AtomicBool,
    put_hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    substitute: std::sync::Mutex<Option<Vec<u8>>>,
    hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl BlobStore for HookBlob {
    fn put_if_absent(&self, d: Digest, s: BytesStream) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if self.fail_put.load(Ordering::SeqCst) {
                return Err(RegistryError::new(
                    "registry.storage",
                    "simulated blob write failure",
                ));
            }
            self.inner.put_if_absent(d, s).await?;
            let hook = self.put_hook.lock().unwrap().take();
            if let Some(h) = hook {
                h();
            }
            Ok(())
        })
    }
    fn get(&self, d: Digest) -> BoxFuture<'_, BytesStream> {
        Box::pin(async move {
            let hook = self.hook.lock().unwrap().take();
            if let Some(h) = hook {
                h();
            }
            let substitute = self.substitute.lock().unwrap().clone();
            if let Some(b) = substitute {
                return Ok(bytes_stream(b));
            }
            self.inner.get(d).await
        })
    }
    fn exists(&self, d: Digest) -> BoxFuture<'_, bool> {
        self.inner.exists(d)
    }
    fn size(&self, d: Digest) -> BoxFuture<'_, u64> {
        self.inner.size(d)
    }
    fn inventory(&self) -> BoxFuture<'_, Vec<Digest>> {
        self.inner.inventory()
    }
}
#[test]
fn dishonest_transport_never_returns_wrong_bytes() {
    let blobs = Arc::new(HookBlob::default());
    let registry = Registry::new(
        Arc::new(MemoryIndexStore::default()),
        blobs.clone(),
        RegistryPolicy::default(),
        trust(),
        Arc::new(MemoryAuditSink::default()),
    );
    let r = publish(&registry, bytes("demo", "1.0.0", ""), 12).unwrap();
    for bad in [bytes("other", "1.0.0", ""), vec![0; 30]] {
        *blobs.substitute.lock().unwrap() = Some(bad);
        assert_code(
            run(registry.fetch(&r.app, &r.version, NOW)),
            "registry.digest_mismatch",
        );
    }
}
#[test]
fn revocation_during_blob_io_rechecked_before_exposing_bytes() {
    let blobs = Arc::new(HookBlob::default());
    let trust = trust();
    let registry = Registry::new(
        Arc::new(MemoryIndexStore::default()),
        blobs.clone(),
        RegistryPolicy::default(),
        trust.clone(),
        Arc::new(MemoryAuditSink::default()),
    );
    let r = publish(&registry, bytes("demo", "1.0.0", ""), 12).unwrap();
    *blobs.hook.lock().unwrap() = Some(Box::new(move || {
        trust
            .update_revocations(revoke_key(12, 2, 400, false), NOW)
            .unwrap()
    }));
    assert_code(
        run(registry.fetch(&r.app, &r.version, NOW)),
        "signing.key_revoked",
    );
}
#[test]
fn withdrawal_during_blob_io_rechecked_before_exposing_bytes() {
    let blobs = Arc::new(HookBlob::default());
    let index = Arc::new(MemoryIndexStore::default());
    let registry = Registry::new(
        index.clone(),
        blobs.clone(),
        RegistryPolicy::default(),
        trust(),
        Arc::new(MemoryAuditSink::default()),
    );
    let r = publish(&registry, bytes("demo", "1.0.0", ""), 12).unwrap();
    let release = r.clone();
    *blobs.hook.lock().unwrap() = Some(Box::new(move || {
        index
            .remove(
                &release.app,
                &release.version,
                "publisher12",
                Removal {
                    at: NOW,
                    reason: "security".into(),
                },
                true,
            )
            .unwrap();
    }));
    assert_code(
        run(registry.fetch_digest(r.digest, NOW)),
        "registry.withdrawn",
    );
}

#[test]
fn failed_blob_write_never_creates_index_ownership_or_audit() {
    let blobs = Arc::new(HookBlob::default());
    blobs.fail_put.store(true, Ordering::SeqCst);
    let index = Arc::new(MemoryIndexStore::default());
    let audit = Arc::new(MemoryAuditSink::default());
    let registry = Registry::new(
        index.clone(),
        blobs.clone(),
        RegistryPolicy::default(),
        trust(),
        audit.clone(),
    );
    assert_code(
        publish(&registry, bytes("demo", "1.0.0", ""), 12),
        "registry.storage",
    );
    assert!(index.ownership(&app("demo")).unwrap().is_none());
    assert!(index.digests().unwrap().is_empty());
    assert!(run(blobs.inventory()).unwrap().is_empty());
    assert!(audit.records().unwrap().is_empty());
}
#[test]
fn publish_rechecks_current_trust_after_blob_write_before_index() {
    let blobs = Arc::new(HookBlob::default());
    let trust = trust();
    let index = Arc::new(MemoryIndexStore::default());
    let audit = Arc::new(MemoryAuditSink::default());
    let registry = Registry::new(
        index.clone(),
        blobs.clone(),
        RegistryPolicy::default(),
        trust.clone(),
        audit.clone(),
    );
    *blobs.put_hook.lock().unwrap() = Some(Box::new(move || {
        trust
            .update_revocations(revoke_key(12, 2, 400, false), NOW)
            .unwrap()
    }));
    assert_code(
        publish(&registry, bytes("demo", "1.0.0", ""), 12),
        "signing.key_revoked",
    );
    assert!(index.ownership(&app("demo")).unwrap().is_none());
    assert!(audit.records().unwrap().is_empty());
    assert_eq!(run(registry.repair_orphans()).unwrap().len(), 1);
}
#[test]
fn publish_rechecks_ownership_when_transfer_occurs_during_blob_write() {
    let blobs = Arc::new(HookBlob::default());
    let index = Arc::new(MemoryIndexStore::default());
    let audit = Arc::new(MemoryAuditSink::default());
    let registry = Arc::new(Registry::new(
        index.clone(),
        blobs.clone(),
        RegistryPolicy::default(),
        trust(),
        audit.clone(),
    ));
    publish(&registry, bytes("demo", "1.0.0", ""), 12).unwrap();
    let transfer_registry = registry.clone();
    *blobs.put_hook.lock().unwrap() = Some(Box::new(move || {
        transfer_registry
            .transfer(transfer("demo", 12, 13, 0), NOW)
            .unwrap()
    }));
    assert_code(
        publish(&registry, bytes("demo", "2.0.0", ""), 12),
        "registry.not_owner",
    );
    assert_eq!(index.digests().unwrap().len(), 1);
    assert_eq!(audit.records().unwrap().len(), 2);
    assert_eq!(run(registry.repair_orphans()).unwrap().len(), 1);
}

#[test]
fn filesystem_orphan_survives_store_reopen_and_is_reported_without_import() {
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(FailingIndex::default());
    let audit = Arc::new(MemoryAuditSink::default());
    let blobs = Arc::new(FsBlobStore::new(dir.path(), validation()).unwrap());
    let registry = Registry::new(
        index.clone(),
        blobs,
        RegistryPolicy::default(),
        trust(),
        audit.clone(),
    );
    let b = bytes("demo", "1.0.0", "");
    let digest = open(&b).digest();
    assert_code(publish(&registry, b.clone(), 12), "registry.storage");
    drop(registry);
    let reopened = Arc::new(FsBlobStore::new(dir.path(), validation()).unwrap());
    let registry = Registry::new(
        index.clone(),
        reopened.clone(),
        RegistryPolicy::default(),
        trust(),
        audit.clone(),
    );
    assert_eq!(run(registry.repair_orphans()).unwrap(), vec![digest]);
    assert_eq!(
        run(collect_stream(run(reopened.get(digest)).unwrap(), 10000)).unwrap(),
        b
    );
    assert!(index.ownership(&app("demo")).unwrap().is_none());
    assert!(audit.records().unwrap().is_empty());
    index.fail.store(false, Ordering::SeqCst);
    let release = publish(&registry, b, 12).unwrap();
    assert!(run(registry.fetch(&release.app, &release.version, NOW)).is_ok());
    assert!(run(registry.repair_orphans()).unwrap().is_empty());
}
