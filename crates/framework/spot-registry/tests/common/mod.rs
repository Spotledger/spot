#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]
use spot_bundle::{Bundle, BundleBuilder, HostInfo, Limits};
use spot_bundle_signing::*;
use spot_registry::*;
use std::{
    future::Future,
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
};
pub const NOW: i64 = 500;
pub const COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";
pub type TestRegistry = Registry<MemoryIndexStore, MemoryBlobStore>;
struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
pub fn run<F: Future>(f: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut f = pin!(f);
    loop {
        match f.as_mut().poll(&mut context) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}
pub fn root() -> KeyPair {
    generate_keypair(&[11; 32])
}
pub fn key(id: u8) -> KeyPair {
    generate_keypair(&[id; 32])
}
pub fn cert(id: u8) -> SignedCertificate {
    issue_certificate(
        PublisherCertificate {
            publisher_id: format!("publisher{id}"),
            display_name: format!("Publisher {id}"),
            key: key(id).public(),
            valid_from: 100,
            valid_to: 2000,
            issued_by: root().public().key_id(),
            serial: id as u64,
        },
        &root(),
    )
}
pub fn list(generation: u64) -> RevocationList {
    issue_revocation_list(
        RevocationBody {
            generation,
            issued_at: 100,
            issued_by: root().public().key_id(),
            keys: vec![],
            publishers: vec![],
            bundles: vec![],
        },
        &root(),
    )
}
pub fn anchor() -> TrustAnchor {
    TrustAnchor::new(vec![TrustedRoot::new(root().public(), 0, None)])
}
pub fn trust() -> Arc<RegistryTrust> {
    Arc::new(RegistryTrust::new(anchor(), list(1), NOW).unwrap())
}
pub fn validation() -> BlobValidation {
    BlobValidation::default()
}
pub fn app(s: &str) -> AppName {
    s.parse().unwrap()
}
pub fn version(s: &str) -> Version {
    s.parse().unwrap()
}
pub fn manifest(name: &str, version: &str, extra: &str) -> String {
    format!("[app]\nname = {name:?}\nversion = {version:?}\ntitle = {name:?}\n{extra}")
}
pub fn bytes(name: &str, version: &str, extra: &str) -> Vec<u8> {
    BundleBuilder::new(manifest(name, version, extra), COMPONENT)
        .build()
        .unwrap()
}
pub fn open(bytes: &[u8]) -> Bundle {
    Bundle::open(
        bytes,
        &Limits::default(),
        &HostInfo {
            version: Version::new(0, 1, 0),
        },
    )
    .unwrap()
}
pub fn proof(bytes: &[u8], id: u8) -> SignatureRef {
    let b = open(bytes);
    SignatureRef {
        signature: sign_bundle(
            &b.digest(),
            b.manifest().app.name.as_str(),
            &b.manifest().app.version.to_string(),
            &cert(id),
            &key(id),
            150,
        )
        .unwrap(),
        chain: CertificateChain::new(cert(id)),
    }
}
pub struct Setup {
    pub registry: Arc<TestRegistry>,
    pub index: Arc<MemoryIndexStore>,
    pub blobs: Arc<MemoryBlobStore>,
    pub trust: Arc<RegistryTrust>,
    pub audit: Arc<MemoryAuditSink>,
}
impl Setup {
    pub fn new() -> Self {
        Self::policy(RegistryPolicy::default())
    }
    pub fn policy(policy: RegistryPolicy) -> Self {
        let index = Arc::new(MemoryIndexStore::default());
        let blobs = Arc::new(MemoryBlobStore::new(BlobValidation {
            limits: policy.limits,
            host: policy.host.clone(),
        }));
        let trust = trust();
        let audit = Arc::new(MemoryAuditSink::default());
        let registry = Arc::new(Registry::new(
            index.clone(),
            blobs.clone(),
            policy,
            trust.clone(),
            audit.clone(),
        ));
        Self {
            registry,
            index,
            blobs,
            trust,
            audit,
        }
    }
    pub fn publish(&self, name: &str, version: &str, extra: &str, id: u8) -> Release {
        publish(&self.registry, bytes(name, version, extra), id).unwrap()
    }
    pub fn caller(&self, id: u8) -> Caller {
        let b = bytes("identity", "1.0.0", "");
        let p = authorization(&b, id, NOW);
        self.registry
            .authenticate(&b, p.signature, p.chain, NOW)
            .unwrap()
    }
}
pub fn authorization(bytes: &[u8], id: u8, signed_at: i64) -> SignatureRef {
    SignatureRef {
        signature: sign_bundle(
            &open(bytes).digest(),
            AUTHORIZATION_APP_ID,
            AUTHORIZATION_VERSION,
            &cert(id),
            &key(id),
            signed_at,
        )
        .unwrap(),
        chain: CertificateChain::new(cert(id)),
    }
}
pub fn publish<I: IndexStore, B: BlobStore>(
    registry: &Registry<I, B>,
    bytes: Vec<u8>,
    id: u8,
) -> Result<Release> {
    let p = proof(&bytes, id);
    run(registry.publish(bytes, p.signature, p.chain, NOW))
}
pub fn assert_code<T>(result: Result<T>, code: &str) {
    match result {
        Err(e) => assert_eq!(e.code, code),
        Ok(_) => panic!("expected {code}, got success"),
    }
}
pub fn transfer(app: &str, from: u8, to: u8, generation: u64) -> TransferRecord {
    let request = TransferRequest {
        app: self::app(app),
        from: format!("publisher{from}"),
        to: format!("publisher{to}"),
        generation,
        at: NOW,
        expires_at: 1000,
        reason: "explicit sale".into(),
    };
    let digest = request.digest().unwrap();
    let proof = |id| SignatureRef {
        signature: sign_bundle(
            &digest,
            TRANSFER_APP_ID,
            TRANSFER_VERSION,
            &cert(id),
            &key(id),
            NOW,
        )
        .unwrap(),
        chain: CertificateChain::new(cert(id)),
    };
    TransferRecord {
        request,
        from: proof(from),
        to: proof(to),
    }
}
pub fn revoke_key(
    id: u8,
    generation: u64,
    effective_from: i64,
    compromised: bool,
) -> RevocationList {
    let mut body = list(generation).body;
    body.keys.push(RevokedKey {
        key_id: key(id).public().key_id(),
        effective_from,
        compromised,
        reason: "retired".into(),
    });
    issue_revocation_list(body, &root())
}
