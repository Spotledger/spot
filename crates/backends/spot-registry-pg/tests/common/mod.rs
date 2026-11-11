//! Shared setup: signed fixtures (the same construction the registry crate's own tests use), a
//! scratch database, a bridge on a lazily built pool, and the store with its tables created.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_bundle::{Bundle, BundleBuilder, HostInfo, Limits};
use spot_bundle_signing::*;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_registry::*;
use spot_registry_pg::PgIndexStore;

pub const NOW: i64 = 500;
pub const COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";
pub type TestRegistry<I> = Registry<I, MemoryBlobStore>;

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

static NAMES: AtomicUsize = AtomicUsize::new(0);

pub fn pool_of(config: PgConfig, size: usize) -> Pool {
    let mut config = config;
    config.pool_max_size = size;
    PgBackend::connect_unchecked(config)
        .expect("backend")
        .pool()
}

/// A bridge on its own pool of 10 (in flight 6), and the store on it with its tables created.
pub fn open_db(config: PgConfig) -> (Arc<Bridge>, Arc<PgIndexStore>) {
    let pool = pool_of(config, 10);
    let bridge = Arc::new(
        Bridge::new(
            BridgeConfig {
                name: format!("registry_pg_{}", NAMES.fetch_add(1, Ordering::SeqCst)),
                workers: 2,
                max_in_flight: 6,
                queue_capacity: 32,
                default_deadline: Duration::from_secs(5),
                max_deadline: Duration::from_secs(20),
            },
            pool.clone(),
        )
        .expect("bridge"),
    );
    let store = Arc::new(PgIndexStore::new(Arc::clone(&bridge), &pool).expect("store"));
    store.prepare().expect("prepare");
    (bridge, store)
}

// ---------------------------------------------------------------------------
// Signed fixtures
// ---------------------------------------------------------------------------

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
pub fn trust() -> Arc<RegistryTrust> {
    Arc::new(
        RegistryTrust::new(
            TrustAnchor::new(vec![TrustedRoot::new(root().public(), 0, None)]),
            list(1),
            NOW,
        )
        .unwrap(),
    )
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
pub fn publisher(id: u8) -> Publisher {
    Publisher {
        id: format!("publisher{id}"),
        display_name: format!("Publisher {id}"),
        certificate_chain_root: root().public().key_id(),
        status: PublisherStatus::Active,
    }
}
pub fn assert_code<T: std::fmt::Debug>(result: Result<T>, code: &str) {
    match result {
        Err(e) => assert_eq!(e.code, code, "{e}"),
        Ok(v) => panic!("expected {code}, got success: {v:?}"),
    }
}

/// A registry over any index, with an in-memory blob store: the source of real signed releases
/// and of authenticated callers.
pub struct Setup<I: IndexStore> {
    pub registry: Arc<TestRegistry<I>>,
    pub index: Arc<I>,
    pub blobs: Arc<MemoryBlobStore>,
    pub trust: Arc<RegistryTrust>,
    pub audit: Arc<MemoryAuditSink>,
}

impl<I: IndexStore + 'static> Setup<I> {
    pub fn new(index: Arc<I>) -> Self {
        Self::with_policy(index, RegistryPolicy::default())
    }

    pub fn with_policy(index: Arc<I>, policy: RegistryPolicy) -> Self {
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

    /// Another registry on the same index and blobs (a second instance).
    pub fn sibling(&self) -> Registry<I, MemoryBlobStore> {
        Registry::new(
            self.index.clone(),
            self.blobs.clone(),
            RegistryPolicy::default(),
            self.trust.clone(),
            self.audit.clone(),
        )
    }

    pub fn publish(&self, name: &str, version: &str, extra: &str, id: u8) -> Release {
        let bytes = bytes(name, version, extra);
        let p = proof(&bytes, id);
        run(self.registry.publish(bytes, p.signature, p.chain, NOW)).unwrap()
    }

    pub fn caller(&self, id: u8) -> Caller {
        let b = bytes("identity", "1.0.0", "");
        let p = authorization(&b, id, NOW);
        self.registry
            .authenticate(&b, p.signature, p.chain, NOW)
            .unwrap()
    }
}

pub fn transfer(app_name: &str, from: u8, to: u8, generation: u64) -> TransferRecord {
    let request = TransferRequest {
        app: app(app_name),
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
