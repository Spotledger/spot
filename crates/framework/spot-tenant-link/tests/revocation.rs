//! Revocation and expiry: immediate for new exchanges, honest about what was already in flight,
//! audited, and refused when the audit cannot be written.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{linked, topic};
use spot_audit_chain::AuditRecord;
use spot_tenant_link::{
    AuditSink, DeliveryState, Direction, Error, LinkService, LinkState, LinkStore, MemoryAuditLog,
    MemoryLinkStore, RevocationOutcome, RevokeReason, after_revocation,
};

/// An audit sink that never writes: the change it was asked to record must not happen.
struct FailingAudit;

impl AuditSink for FailingAudit {
    fn append(&self, _record: AuditRecord) -> spot_tenant_link::Result<u64> {
        Err(Error::AuditFailed {
            action: "the test audit",
        })
    }
}

#[test]
fn either_side_can_revoke_and_new_exchanges_stop_immediately() {
    let store = MemoryLinkStore::new();
    let audit = MemoryAuditLog::new();
    let service = LinkService::new(&store, &audit);
    let link = store.insert(linked().a).unwrap();
    // This side revokes its copy; the very next check refuses.
    assert!(
        service
            .authorize(&link.id, &topic(), Direction::Send, 1_900, "user:a", None)
            .is_ok()
    );
    let revoked = service
        .revoke(&link.id, RevokeReason::SecurityConcern, "user:b", 2_000)
        .unwrap();
    assert_eq!(revoked.state, LinkState::Revoked);
    assert_eq!(revoked.revocation.unwrap().by, link.local);
    assert_eq!(revoked.version, 2);
    assert!(matches!(
        service.authorize(&link.id, &topic(), Direction::Send, 2_001, "user:a", None),
        Err(Error::Revoked { .. })
    ));
    let events = audit.events().unwrap();
    let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
    assert_eq!(
        kinds,
        vec![
            spot_tenant_link::LinkEventKind::Allowed,
            spot_tenant_link::LinkEventKind::Revoked,
            spot_tenant_link::LinkEventKind::Refused,
        ]
    );
    assert_eq!(events[1].actor, "user:b");
}

#[test]
fn acknowledged_messages_stay_and_unsent_ones_are_held_revoked() {
    for state in [
        DeliveryState::Acknowledged,
        DeliveryState::Applied,
        DeliveryState::Rejected,
    ] {
        assert_eq!(after_revocation(state), RevocationOutcome::Stays);
    }
    for state in [DeliveryState::Queued, DeliveryState::Sent] {
        assert_eq!(after_revocation(state), RevocationOutcome::HeldRevoked);
    }
    assert_eq!(
        after_revocation(DeliveryState::Expired),
        RevocationOutcome::AlreadyExpired
    );
}

#[test]
fn expiry_is_effective_and_audited() {
    let store = MemoryLinkStore::new();
    let audit = MemoryAuditLog::new();
    let service = LinkService::new(&store, &audit);
    let mut link = linked().a;
    link.expires = Some(2_000);
    let link = store.insert(link).unwrap();
    assert!(matches!(
        link.allows(&topic(), Direction::Send, 2_000),
        Err(Error::LinkExpired { .. })
    ));
    assert!(matches!(
        service.expire(&link.id, 1_999),
        Err(Error::InvalidExpiry { .. })
    ));
    let expired = service.expire(&link.id, 2_000).unwrap();
    assert_eq!(expired.state, LinkState::Expired);
    let events = audit.events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, spot_tenant_link::LinkEventKind::Expired);
}

#[test]
fn a_revoked_link_is_not_the_live_link_of_the_pair() {
    let store = MemoryLinkStore::new();
    let link = store.insert(linked().a).unwrap();
    let peer = link.parties[1].clone();
    let revoked = link
        .revoked_by(&peer, RevokeReason::TermsEnded, 1_900)
        .unwrap();
    store.update(revoked, 1).unwrap();
    assert!(
        store
            .find(&link.parties[0], &link.parties[1])
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_stale_version_cannot_overwrite_a_revocation() {
    let store = MemoryLinkStore::new();
    let link = store.insert(linked().a).unwrap();
    let peer = link.parties[1].clone();
    let revoked = link
        .revoked_by(&peer, RevokeReason::SecurityConcern, 1_900)
        .unwrap();
    store.update(revoked, 1).unwrap();
    // A second writer that read version 1 tries to write its own change.
    let stale = link
        .revoked_by(&link.parties[0], RevokeReason::TermsEnded, 1_950)
        .unwrap();
    assert!(matches!(
        store.update(stale, 1),
        Err(Error::VersionConflict { .. })
    ));
}

#[test]
fn a_change_is_not_persisted_when_its_audit_entry_cannot_be_written() {
    let store = MemoryLinkStore::new();
    let link = store.insert(linked().a).unwrap();
    let service = LinkService::new(&store, &FailingAudit);
    assert!(matches!(
        service.revoke(&link.id, RevokeReason::TermsEnded, "user:b", 2_000),
        Err(Error::AuditFailed { .. })
    ));
    assert_eq!(
        store.get(&link.id).unwrap().state,
        LinkState::Active,
        "the refused audit left the link untouched"
    );
}

#[test]
fn revoking_one_copy_stops_that_side_and_its_own_trail_says_so() {
    let left = linked();
    let right = linked();
    let left_store = MemoryLinkStore::new();
    let right_store = MemoryLinkStore::new();
    let left_audit = MemoryAuditLog::new();
    let right_audit = MemoryAuditLog::new();
    let left_link = left_store.insert(left.a).unwrap();
    let right_link = right_store.insert(right.b).unwrap();
    LinkService::new(&left_store, &left_audit)
        .revoke(&left_link.id, RevokeReason::TermsEnded, "user:b", 2_000)
        .unwrap();
    assert!(matches!(
        LinkService::new(&left_store, &left_audit).authorize(
            &left_link.id,
            &topic(),
            Direction::Send,
            2_001,
            "user:a",
            None,
        ),
        Err(Error::Revoked { .. })
    ));
    // The other copy is untouched until its own side records the change: no shared ledger.
    assert!(
        LinkService::new(&right_store, &right_audit)
            .authorize(
                &right_link.id,
                &topic(),
                Direction::Receive,
                2_001,
                "user:b",
                None,
            )
            .is_ok()
    );
    assert_eq!(
        left_audit.events().unwrap().len(),
        2,
        "revoked and refused, in that order"
    );
}

#[test]
fn one_grant_can_expire_while_another_still_runs() {
    use common::grant;

    let mut link = linked().a;
    let mut short = grant(Direction::Send);
    short.topic = spot_tenant_link::Topic::new("bpm.short").unwrap();
    short.expires = 5_000;
    let mut long = grant(Direction::Send);
    long.topic = spot_tenant_link::Topic::new("bpm.long").unwrap();
    long.expires = 9_000;
    link.grants = vec![short, long];
    link.state = LinkState::Active;
    assert!(matches!(
        link.allows(
            &spot_tenant_link::Topic::new("bpm.short").unwrap(),
            Direction::Send,
            5_000
        ),
        Err(Error::GrantExpired { .. })
    ));
    assert!(
        link.allows(
            &spot_tenant_link::Topic::new("bpm.long").unwrap(),
            Direction::Send,
            6_000
        )
        .is_ok()
    );
}
