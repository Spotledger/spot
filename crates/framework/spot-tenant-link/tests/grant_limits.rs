//! WP-CTL-09.T11: one grant can be withdrawn without the link, and an inbound message has a byte
//! bound that is checked before anything is parsed or allocated.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::io::Read;

use common::{linked, topic};
use spot_tenant_link::{
    Direction, Error, GrantRevocations, LinkService, LinkStore, MemoryAuditLog,
    MemoryGrantRevocations, MemoryLinkStore, Topic, WireLimits,
};

fn second_topic() -> Topic {
    Topic::new("bpm.second").unwrap()
}

fn two_grant_link() -> spot_tenant_link::Link {
    let mut link = linked().a;
    let mut extra = link.grants[0].clone();
    extra.topic = second_topic();
    link.grants.push(extra);
    link
}

#[test]
fn a_revoked_grant_is_refused_and_its_sibling_is_accepted() {
    let store = MemoryLinkStore::new();
    let audit = MemoryAuditLog::new();
    let revocations = MemoryGrantRevocations::new();
    let service = LinkService::new(&store, &audit).with_revocations(&revocations);
    let link = store.insert(two_grant_link()).unwrap();

    assert!(
        service
            .authorize(&link.id, &topic(), Direction::Send, 1_900, "user:a", None)
            .is_ok()
    );
    service
        .revoke_grant(&link.id, &topic(), "user:admin", 1_950)
        .unwrap();
    assert!(revocations.is_revoked(&link.id, &topic()));

    assert!(matches!(
        service.authorize(&link.id, &topic(), Direction::Send, 1_960, "user:a", None),
        Err(Error::TopicNotGranted { .. })
    ));
    // The sibling grant and the link itself are untouched.
    assert!(
        service
            .authorize(
                &link.id,
                &second_topic(),
                Direction::Send,
                1_960,
                "user:a",
                None
            )
            .is_ok()
    );
    assert_eq!(store.get(&link.id).unwrap().state, link.state);
    // The refusal of the withdrawn grant and the withdrawal are in the trail.
    let kinds: Vec<_> = audit.events().unwrap().iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&spot_tenant_link::LinkEventKind::Refused));
    assert!(kinds.contains(&spot_tenant_link::LinkEventKind::Revoked));
}

#[test]
fn revoking_needs_a_list_and_a_grant_that_exists_and_a_service_without_a_list_is_unchanged() {
    let store = MemoryLinkStore::new();
    let audit = MemoryAuditLog::new();
    let link = store.insert(two_grant_link()).unwrap();
    let plain = LinkService::new(&store, &audit);
    assert!(
        plain
            .revoke_grant(&link.id, &topic(), "user:a", 2_000)
            .is_err()
    );
    assert!(
        plain
            .authorize(&link.id, &topic(), Direction::Send, 1_900, "user:a", None)
            .is_ok()
    );

    let revocations = MemoryGrantRevocations::new();
    let service = LinkService::new(&store, &audit).with_revocations(&revocations);
    let unknown = Topic::new("bpm.unknown").unwrap();
    assert!(matches!(
        service.revoke_grant(&link.id, &unknown, "user:a", 2_000),
        Err(Error::TopicNotGranted { .. })
    ));
    assert!(revocations.is_empty());
}

#[test]
fn the_size_bound_has_exact_boundaries() {
    let limits = WireLimits::new(1_000).unwrap();
    limits.check(0).unwrap();
    limits.check(999).unwrap();
    limits.check(1_000).unwrap();
    assert!(matches!(
        limits.check(1_001),
        Err(Error::MessageTooLarge { limit: 1_000 })
    ));
    assert!(WireLimits::new(0).is_err());
    assert!(WireLimits::new(usize::MAX).is_err());
    assert_eq!(WireLimits::default().max_message_bytes(), 256 * 1024);
}

/// A reader of an unbounded stream that counts how much it was asked for.
struct Endless(std::cell::Cell<u64>);

impl Read for Endless {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.set(self.0.get() + buf.len() as u64);
        buf.fill(b'x');
        Ok(buf.len())
    }
}

#[test]
fn an_oversize_message_is_refused_without_reading_its_size() {
    let limits = WireLimits::new(4_096).unwrap();
    let mut endless = Endless(std::cell::Cell::new(0));
    let refused = limits.read(&mut endless);
    assert!(matches!(
        refused,
        Err(Error::MessageTooLarge { limit: 4_096 })
    ));
    // The reader was asked for the bound plus a read buffer's slack, not for the stream's size.
    assert!(
        endless.0.get() < 1 << 20,
        "{} bytes were requested",
        endless.0.get()
    );

    let exactly = vec![b'y'; 4_096];
    assert_eq!(limits.read(&exactly[..]).unwrap().len(), 4_096);
    let over = vec![b'y'; 4_097];
    assert!(limits.read(&over[..]).is_err());
}
