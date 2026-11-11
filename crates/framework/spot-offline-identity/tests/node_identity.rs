//! Node identity: certificate lifecycle, suspension, and key release.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_offline_identity::*;

#[test]
fn a_certificate_renews_only_while_connected_and_lapses_alone() {
    let mut cert = CertLifecycle::none(clock(0, 1));
    assert!(!cert.is_valid(clock(0, 1)));
    assert_eq!(
        cert.renew(1, HOUR, false, clock(0, 1)),
        Err(OfflineError::CertificateOffline)
    );
    cert.renew(1, HOUR, true, clock(0, 1)).unwrap();
    assert!(cert.is_valid(clock(HOUR - 1, 1)));
    assert!(!cert.is_valid(clock(HOUR, 1)), "it lapses by itself");
    assert_eq!(cert.remaining_ms(clock(HOUR + 5, 1)), 0);
}

#[test]
fn certificate_lifetime_and_serial_are_checked() {
    let mut cert = CertLifecycle::none(clock(0, 1));
    assert_eq!(
        cert.renew(1, 0, true, clock(0, 1)),
        Err(OfflineError::CertificateLifetime)
    );
    assert_eq!(
        cert.renew(1, MAX_CERT_TTL_MS + 1, true, clock(0, 1)),
        Err(OfflineError::CertificateLifetime)
    );
    cert.renew(5, HOUR, true, clock(0, 1)).unwrap();
    assert_eq!(
        cert.renew(5, HOUR, true, clock(1, 1)),
        Err(OfflineError::CertificateSerial)
    );
    assert_eq!(
        cert.renew(4, HOUR, true, clock(1, 1)),
        Err(OfflineError::CertificateSerial)
    );
    cert.renew(6, HOUR, true, clock(1, 1)).unwrap();
}

#[test]
fn a_reboot_does_not_extend_the_certificate() {
    let w = World::new();
    let mut s = w.store_with(snapshot(1));
    s.renew_certificate(2, HOUR, true, clock(0, 1)).unwrap();
    s.checkpoint(clock(40 * 60_000, 1)).unwrap();
    let r = SnapshotStore::open(w.anchor(), s.into_persistence(), clock(0, 2));
    assert!(r.certificate_valid(clock(19 * 60_000, 2)));
    assert!(!r.certificate_valid(clock(20 * 60_000, 2)));
}

fn registry() -> NodeRegistry {
    let mut r = NodeRegistry::new(tenant());
    r.register(node());
    r.register(NodeId::new("branch-2").unwrap());
    r
}

#[test]
fn suspension_and_decommission_publish_revocation_entries() {
    let mut r = registry();
    assert!(r.revocation_entries().is_empty());
    r.suspend(&node(), 7).unwrap();
    r.decommission(&NodeId::new("branch-2").unwrap(), 8)
        .unwrap();
    let entries = r.revocation_entries();
    assert_eq!(
        entries,
        vec![
            Revocation::Node {
                node: "branch-1".into(),
                kind: NodeRevocationKind::Suspended,
                effective_epoch: 7
            },
            Revocation::Node {
                node: "branch-2".into(),
                kind: NodeRevocationKind::Decommissioned,
                effective_epoch: 8
            },
        ]
    );
    // The entries travel in the next snapshot, and the other nodes learn from it.
    let mut snap = snapshot(2);
    snap.revocations = entries;
    snap.node = node();
    let w = World::new();
    let s = w.store_with(snap);
    assert_eq!(
        s.sign_in("clerk", clock(1, 1)),
        Decision::Deny(DenyReason::NodeSuspended)
    );
}

#[test]
fn reinstating_works_for_suspended_but_decommissioned_is_final() {
    let mut r = registry();
    r.suspend(&node(), 3).unwrap();
    r.reinstate(&node()).unwrap();
    assert_eq!(r.state(&node()), Some(NodeState::Active));
    assert!(r.revocation_entries().is_empty());
    let b2 = NodeId::new("branch-2").unwrap();
    r.decommission(&b2, 4).unwrap();
    assert!(r.reinstate(&b2).is_err());
    r.suspend(&b2, 9).unwrap(); // cannot soften a decommission
    assert_eq!(r.state(&b2), Some(NodeState::Decommissioned));
    r.register(b2.clone()); // registering again does not reactivate
    assert_eq!(r.state(&b2), Some(NodeState::Decommissioned));
    assert_eq!(
        r.suspend(&NodeId::new("ghost").unwrap(), 1),
        Err(OfflineError::UnknownNode)
    );
}

struct Inner {
    calls: usize,
}

impl KeyReleasePort for Inner {
    fn release(&mut self, _r: &KeyReleaseRequest) -> Result<Vec<u8>, OfflineError> {
        self.calls += 1;
        Ok(vec![1, 2, 3]) // stands for the key wrapped to the node: ciphertext
    }
}

fn request(claimed: &str, authenticated: &str) -> KeyReleaseRequest {
    KeyReleaseRequest {
        tenant: tenant(),
        claimed_node: NodeId::new(claimed).unwrap(),
        authenticated_node: NodeId::new(authenticated).unwrap(),
        group: "bank_details".to_owned(),
        at_micros: 5,
    }
}

#[test]
fn an_active_node_gets_its_key_and_the_release_is_audited() {
    let r = registry();
    let mut gate = GatedKeyRelease::new(&r, Inner { calls: 0 }, MemoryAuditSink::default());
    assert_eq!(
        gate.release(&request("branch-1", "branch-1")).unwrap(),
        vec![1, 2, 3]
    );
    assert_eq!(gate.audit().records.len(), 1);
    assert_eq!(
        gate.audit().records[0].kind,
        "offline_identity.key_release_allowed"
    );
}

#[test]
fn a_suspended_node_gets_no_further_keys() {
    let mut r = registry();
    r.suspend(&node(), 2).unwrap();
    let mut gate = GatedKeyRelease::new(&r, Inner { calls: 0 }, MemoryAuditSink::default());
    let err = gate.release(&request("branch-1", "branch-1")).unwrap_err();
    assert!(matches!(err, OfflineError::KeyReleaseRefused { .. }));
    assert_eq!(err.code(), "offline_identity.key_release_refused");
    assert_eq!(
        gate.audit().records[0].kind,
        "offline_identity.key_release_refused"
    );
}

#[test]
fn decommissioned_unknown_and_impersonating_nodes_get_nothing() {
    let mut r = registry();
    r.decommission(&NodeId::new("branch-2").unwrap(), 2)
        .unwrap();
    let inner_calls =
        |gate: &GatedKeyRelease<'_, Inner, MemoryAuditSink>| gate.audit().records.len();
    let mut gate = GatedKeyRelease::new(&r, Inner { calls: 0 }, MemoryAuditSink::default());
    assert!(gate.release(&request("branch-2", "branch-2")).is_err());
    assert!(gate.release(&request("ghost", "ghost")).is_err());
    // A node authenticated as branch-2 claiming to be the healthy branch-1.
    assert!(gate.release(&request("branch-1", "branch-2")).is_err());
    // And the reverse: branch-1 claiming another node's keys.
    assert!(gate.release(&request("branch-2", "branch-1")).is_err());
    assert_eq!(inner_calls(&gate), 4, "every refusal is audited");
    let mut other = request("branch-1", "branch-1");
    other.tenant = TenantId::new("globex").unwrap();
    assert!(gate.release(&other).is_err());
}

#[test]
fn if_the_audit_cannot_be_written_no_key_leaves() {
    let r = registry();
    let mut gate = GatedKeyRelease::new(
        &r,
        Inner { calls: 0 },
        MemoryAuditSink {
            records: vec![],
            fail: true,
        },
    );
    assert_eq!(
        gate.release(&request("branch-1", "branch-1")),
        Err(OfflineError::AuditFailed)
    );
}
