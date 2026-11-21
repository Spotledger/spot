//! The enrolment contract on the in-memory store, plus the properties that need no database:
//! concurrent redemption, two-tenant isolation, audit atomicity, no secrets in any text, the
//! node identity files and the proof of possession.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;
mod contract;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{TestClock, identity, node, request, rig};
use contract::client_cert;
use spot_sync_enrol::{
    EnrolError, EnrolmentStore, MemoryEnrolmentStore, NodeIdentity, ParsedCert, PinnedCert,
};
use spot_sync_http::NodeDirectory;

fn memory() -> Arc<dyn EnrolmentStore> {
    Arc::new(MemoryEnrolmentStore::new())
}

#[test]
fn the_memory_store_passes_the_contract() {
    contract::run_all(&memory);
}

#[test]
fn of_twenty_concurrent_redemptions_of_one_code_exactly_one_wins() {
    let rig = Arc::new(rig(memory(), TestClock::new()));
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_c"))
        .unwrap();
    let wins = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for i in 0..20 {
        let (rig, wins, code) = (Arc::clone(&rig), Arc::clone(&wins), issued.code.clone());
        handles.push(std::thread::spawn(move || {
            let id = identity("edge_c");
            let outcome = rig.service.redeem(
                &code,
                &node("edge_c"),
                id.certificate_der(),
                &format!("src{i}"),
            );
            match outcome {
                Ok(_) => {
                    wins.fetch_add(1, Ordering::SeqCst);
                }
                Err(error) => assert_eq!(error.code(), "sync_enrol.code_invalid"),
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(wins.load(Ordering::SeqCst), 1);
    assert_eq!(rig.service.list().unwrap().len(), 1);
}

#[test]
fn a_certificate_pinned_in_two_tenants_authenticates_only_where_it_is_enrolled() {
    let a = rig(memory(), TestClock::new());
    let b = rig(memory(), TestClock::new());
    let id = identity("edge_t");
    let issued = a
        .service
        .create_enrolment("admin", &request("edge_t"))
        .unwrap();
    a.service
        .redeem(&issued.code, &node("edge_t"), id.certificate_der(), "s")
        .unwrap();
    assert!(a.directory.resolve(&client_cert(&id)).is_ok());
    assert!(
        b.directory.resolve(&client_cert(&id)).is_err(),
        "not enrolled in B"
    );
    // Enrol the same certificate in B as well (the same node name, its own code).
    let issued_b = b
        .service
        .create_enrolment("admin", &request("edge_t"))
        .unwrap();
    b.service
        .redeem(&issued_b.code, &node("edge_t"), id.certificate_der(), "s")
        .unwrap();
    assert!(b.directory.resolve(&client_cert(&id)).is_ok());
    // Revoking in A leaves B alone.
    a.service.revoke("admin", &node("edge_t")).unwrap();
    assert!(a.directory.resolve(&client_cert(&id)).is_err());
    assert!(b.directory.resolve(&client_cert(&id)).is_ok());
    // A code from A is nothing in B.
    assert_eq!(
        b.service
            .redeem(
                &issued.code,
                &node("edge_t"),
                identity("edge_t").certificate_der(),
                "s2"
            )
            .unwrap_err(),
        EnrolError::CodeInvalid
    );
}

/// An audit record that cannot be written means the change does not exist.
#[test]
fn a_failed_audit_write_leaves_nothing_behind() {
    let store = Arc::new(MemoryEnrolmentStore::new());
    let rig = rig(store.clone(), TestClock::new());
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_a"))
        .unwrap();
    store.fail_audit_writes(true);
    let id = identity("edge_a");
    assert_eq!(
        rig.service
            .redeem(&issued.code, &node("edge_a"), id.certificate_der(), "s")
            .unwrap_err()
            .code(),
        "sync_enrol.store_failed"
    );
    assert!(
        rig.service
            .create_enrolment("admin", &request("edge_b"))
            .is_err()
    );
    assert!(
        rig.service.list().unwrap().is_empty(),
        "no node was created"
    );
    store.fail_audit_writes(false);
    // The code was not consumed by the failed attempt, and the enrolment for edge_b does not exist.
    rig.service
        .redeem(&issued.code, &node("edge_a"), id.certificate_der(), "s")
        .unwrap();
    assert_eq!(
        store.audit_log().len(),
        2,
        "enrolled and redeemed, nothing else"
    );
}

#[test]
fn no_text_carries_a_code_a_hash_a_salt_or_a_key() {
    let store = Arc::new(MemoryEnrolmentStore::new());
    let rig = rig(store.clone(), TestClock::new());
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_s"))
        .unwrap();
    let id = identity("edge_s");
    rig.service
        .redeem(&issued.code, &node("edge_s"), id.certificate_der(), "s")
        .unwrap();
    let secret = issued.code.split_once('.').unwrap().1.to_owned();
    let mut shown = format!(
        "{:?} {:?} {:?} {:?}",
        issued, rig.service, rig.directory, id
    );
    shown.push_str(&format!("{:?}", store.audit_log()));
    for text in ["", "x"] {
        let e = rig.service.redeem(
            &format!("{}.{secret}", issued.id),
            &node("edge_s"),
            id.certificate_der(),
            text,
        );
        shown.push_str(&format!("{e:?}"));
    }
    assert!(!shown.contains(&secret), "the code's secret leaked");
    let pkcs8_hex: String = id.private_key_pkcs8()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(!shown.contains(&pkcs8_hex), "the key leaked");
}

/// A store that can be switched off for snapshots, delegating everything else.
struct Flaky {
    inner: MemoryEnrolmentStore,
    down: std::sync::atomic::AtomicBool,
}

impl EnrolmentStore for Flaky {
    fn create_enrolment(
        &self,
        new: &spot_sync_enrol::NewEnrolment,
        audit: &spot_sync_enrol::AuditEvent,
    ) -> spot_sync_enrol::Result<()> {
        self.inner.create_enrolment(new, audit)
    }
    fn redeem(
        &self,
        request: &spot_sync_enrol::RedeemRequest<'_>,
        audit: &spot_sync_enrol::AuditEvent,
    ) -> spot_sync_enrol::Result<spot_sync_enrol::Redeemed> {
        self.inner.redeem(request, audit)
    }
    fn rotate(
        &self,
        rotation: &spot_sync_enrol::Rotation<'_>,
        audit: &spot_sync_enrol::AuditEvent,
    ) -> spot_sync_enrol::Result<()> {
        self.inner.rotate(rotation, audit)
    }
    fn set_status(
        &self,
        node: &spot_sync_protocol::NodeId,
        to: spot_sync_enrol::NodeStatus,
        now: i64,
        audit: &spot_sync_enrol::AuditEvent,
    ) -> spot_sync_enrol::Result<()> {
        self.inner.set_status(node, to, now, audit)
    }
    fn list(&self) -> spot_sync_enrol::Result<Vec<spot_sync_enrol::NodeSummary>> {
        self.inner.list()
    }
    fn snapshot(&self, now: i64) -> spot_sync_enrol::Result<Vec<PinnedCert>> {
        if self.down.load(Ordering::SeqCst) {
            Err(EnrolError::StoreFailed { reason: "down" })
        } else {
            self.inner.snapshot(now)
        }
    }
    fn active_certificate(
        &self,
        node: &spot_sync_protocol::NodeId,
    ) -> spot_sync_enrol::Result<Vec<u8>> {
        self.inner.active_certificate(node)
    }
    fn expiring(&self, now: i64) -> spot_sync_enrol::Result<Vec<spot_sync_enrol::ExpiringCert>> {
        self.inner.expiring(now)
    }
}

#[test]
fn a_store_outage_keeps_the_old_snapshot_until_it_is_too_old() {
    let flaky = Arc::new(Flaky {
        inner: MemoryEnrolmentStore::new(),
        down: std::sync::atomic::AtomicBool::new(false),
    });
    let rig = rig(flaky.clone(), TestClock::new());
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_f"))
        .unwrap();
    let id = identity("edge_f");
    rig.service
        .redeem(&issued.code, &node("edge_f"), id.certificate_der(), "s")
        .unwrap();
    flaky.down.store(true, Ordering::SeqCst);
    rig.clock.advance(10);
    assert!(rig.directory.refresh().is_err(), "the failure is reported");
    assert!(
        rig.directory.resolve(&client_cert(&id)).is_ok(),
        "the old snapshot still serves"
    );
    rig.clock.advance(25);
    assert!(rig.directory.refresh().is_err());
    assert_eq!(
        rig.directory.resolve(&client_cert(&id)).unwrap_err().code,
        "sync_http.identity_unknown",
        "an unreadable directory authenticates nobody"
    );
    flaky.down.store(false, Ordering::SeqCst);
    rig.directory.refresh().unwrap();
    assert!(rig.directory.resolve(&client_cert(&id)).is_ok());
}

#[test]
fn the_node_identity_files_are_private_and_a_loose_key_file_is_refused() {
    let dir = std::env::temp_dir().join(format!("spot-enrol-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let created = NodeIdentity::load_or_create(&dir, &node("edge_k"), common::T0).unwrap();
    let again = NodeIdentity::load_or_create(&dir, &node("edge_k"), common::T0 + 99).unwrap();
    assert_eq!(
        created.fingerprint(),
        again.fingerprint(),
        "the stored identity is reused"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let key = dir.join("node.key");
        let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        for loose in [0o640, 0o604, 0o644, 0o660] {
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(loose)).unwrap();
            assert_eq!(
                NodeIdentity::load_or_create(&dir, &node("edge_k"), common::T0).unwrap_err(),
                EnrolError::KeyFilePermissions,
                "{loose:o}"
            );
        }
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(NodeIdentity::load_or_create(&dir, &node("edge_k"), common::T0).is_ok());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn certificates_outside_their_validity_or_unreadable_are_refused_at_the_door() {
    let now = common::T0;
    for (from, to) in [
        (now - 1_000_000, now - 1_000),
        (now + 1_000, now + 1_000_000),
    ] {
        let der = common::cert_between("edge_v", from, to);
        assert_eq!(
            ParsedCert::parse(&der, now).unwrap_err(),
            EnrolError::CertInvalid
        );
    }
    // Within the five-minute skew it is accepted.
    let edge = common::cert_between("edge_v", now + 200, now + 1_000_000);
    assert!(ParsedCert::parse(&edge, now).is_ok());
    assert_eq!(
        ParsedCert::parse(b"not a certificate", now).unwrap_err(),
        EnrolError::CertInvalid
    );
    assert_eq!(
        ParsedCert::parse(&[], now).unwrap_err(),
        EnrolError::CertInvalid
    );
}

#[test]
fn a_directory_that_was_never_refreshed_authenticates_nobody() {
    let store = memory();
    let clock = TestClock::new();
    let directory = spot_sync_enrol::DurableDirectory::new(
        store,
        clock,
        spot_sync_enrol::DirectoryConfig::default(),
    );
    assert!(directory.needs_refresh());
    assert!(
        directory
            .resolve(&client_cert(&identity("edge_n")))
            .is_err()
    );
}
