//! Shared fixtures: a clock, a deterministic random source and id source, certificates, and the
//! service builder.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use rcgen::{CertificateParams, DnType, KeyPair};
use spot_sync_enrol::{
    Clock, DirectoryConfig, DurableDirectory, EnrolRequest, EnrolService, EnrolmentStore, Ids,
    NodeIdentity, RandomSource,
};
use spot_sync_protocol::NodeId;
use time::{Duration, OffsetDateTime};

/// 2026-01-01T00:00:00Z.
pub const T0: i64 = 1_767_225_600;

pub struct TestClock(pub AtomicI64);

impl TestClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(T0)))
    }
    pub fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
    pub fn set(&self, at: i64) {
        self.0.store(at, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A deterministic byte source: every call continues a counter.
#[derive(Default)]
pub struct CountingRandom(pub AtomicU64);

impl RandomSource for CountingRandom {
    fn fill(&self, buffer: &mut [u8]) {
        for byte in buffer {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            *byte = (n.wrapping_mul(2_654_435_761) >> 7) as u8;
        }
    }
}

#[derive(Default)]
pub struct SeqIds(pub AtomicU64);

impl Ids for SeqIds {
    fn uuid(&self) -> String {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        format!("00000000-0000-4000-8000-{n:012x}")
    }
    fn enrolment_id(&self) -> String {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        format!("enrol_{n}")
    }
}

pub fn node(name: &str) -> NodeId {
    // A node id's site is a lower-case host name: no underscores.
    NodeId::new(&name.replace('_', "-")).unwrap()
}

/// A fresh self-signed identity (the real generator).
pub fn identity(name: &str) -> NodeIdentity {
    NodeIdentity::generate_replacement(&node(name), T0).unwrap()
}

/// A certificate valid from `from` to `to` (Unix seconds), as DER.
pub fn cert_between(name: &str, from: i64, to: i64) -> Vec<u8> {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![name.to_owned()]).unwrap();
    params.distinguished_name.push(DnType::CommonName, name);
    params.not_before = OffsetDateTime::from_unix_timestamp(from).unwrap();
    params.not_after = OffsetDateTime::from_unix_timestamp(to).unwrap();
    params.self_signed(&key).unwrap().der().to_vec()
}

pub struct Rig {
    pub service: EnrolService,
    pub directory: Arc<DurableDirectory>,
    pub store: Arc<dyn EnrolmentStore>,
    pub clock: Arc<TestClock>,
}

pub fn rig(store: Arc<dyn EnrolmentStore>, clock: Arc<TestClock>) -> Rig {
    let directory = Arc::new(DurableDirectory::new(
        Arc::clone(&store),
        clock.clone(),
        DirectoryConfig::default(),
    ));
    let service = EnrolService::new(
        Arc::clone(&store),
        Arc::clone(&directory),
        Arc::new(CountingRandom::default()),
        // The shared test database keeps its outbox between runs: ids start where no earlier run did.
        Arc::new(SeqIds(AtomicU64::new(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0) * 64),
        ))),
        clock.clone(),
    );
    directory.refresh().unwrap();
    Rig {
        service,
        directory,
        store,
        clock,
    }
}

pub fn request(name: &str) -> EnrolRequest {
    EnrolRequest {
        node: node(name),
        role: "edge".to_owned(),
        partitions: vec!["branch_01".to_owned()],
        template: "{}".to_owned(),
        ttl_seconds: None,
    }
}

pub fn _unused(_: Duration) {}
