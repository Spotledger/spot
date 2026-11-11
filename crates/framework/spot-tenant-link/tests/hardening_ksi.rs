//! The register items recorded security findings: a region requirement that no receiver can skip, and a
//! revocation whose party is derived, never named.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{link_id, linked, nonce, tenant, topic};
use spot_data_policy::Region;
use spot_tenant_link::{
    Direction, Error, IdentitySigner, LinkEventKind, LinkService, LinkState, LinkStore,
    MemoryAuditLog, MemoryLinkStore, MemoryNonceLog, MemoryRegionOracle, NonceLog,
    REVOKE_ACTIVE_REVISION, REVOKE_VERSION, RevokeMessage, RevokeReason, SeedSigner,
};

fn peer_signer() -> SeedSigner {
    SeedSigner::from_seed([2u8; 32], tenant(2))
}

fn stranger_signer() -> SeedSigner {
    SeedSigner::from_seed([9u8; 32], tenant(2))
}

fn region_link() -> spot_tenant_link::Link {
    let mut link = linked().a;
    link.grants[0].require_peer_region = Some(Region::new("eu-west").unwrap());
    link
}

// Recorded security finding ---------------------------------------------------------------------------------------

#[test]
fn a_region_restricted_grant_is_refused_without_an_oracle_on_every_public_path() {
    // The receiver's copy: it holds the receive grant.
    let mut link = linked().b;
    link.grants[0].require_peer_region = Some(Region::new("eu-west").unwrap());
    // The plain check every receiver calls.
    let plain = link.allows(&topic(), Direction::Receive, 2_000);
    assert!(
        matches!(plain, Err(Error::PeerRegionMismatch { .. })),
        "{plain:?}"
    );
    // The service path with no oracle.
    let store = MemoryLinkStore::new();
    let audit = MemoryAuditLog::new();
    let stored = store.insert(link).unwrap();
    let service = LinkService::new(&store, &audit);
    let refused = service.authorize(
        &stored.id,
        &topic(),
        Direction::Receive,
        2_000,
        "user:b",
        None,
    );
    assert!(
        matches!(refused, Err(Error::PeerRegionMismatch { .. })),
        "{refused:?}"
    );
    // The refusal is audited like any other.
    assert_eq!(audit.events().unwrap()[0].kind, LinkEventKind::Refused);
    // With an oracle that knows the peer's region the same grant passes.
    let oracle = MemoryRegionOracle::new();
    oracle
        .insert(tenant(1), Region::new("eu-west").unwrap())
        .unwrap();
    assert!(
        service
            .authorize(
                &stored.id,
                &topic(),
                Direction::Receive,
                2_001,
                "user:b",
                Some(&oracle)
            )
            .is_ok()
    );
    assert!(
        stored
            .allows_with_region(&topic(), Direction::Receive, 2_000, &oracle)
            .is_ok()
    );
}

#[test]
fn the_refusal_names_no_region_the_oracle_never_gave() {
    let error = region_link()
        .allows(&topic(), Direction::Send, 2_000)
        .unwrap_err();
    assert_eq!(error.code(), "tenant_link.peer_region_mismatch");
    assert!(error.to_string().contains("unknown"));
}

#[test]
fn a_grant_without_a_requirement_still_passes_the_plain_check() {
    let link = linked().a;
    assert!(link.allows(&topic(), Direction::Send, 2_000).is_ok());
}

// Recorded security finding ---------------------------------------------------------------------------------------

struct World {
    store: MemoryLinkStore,
    audit: MemoryAuditLog,
    nonces: MemoryNonceLog,
    id: spot_tenant_link::LinkId,
}

fn world() -> World {
    world_with(linked().a)
}

fn world_with(link: spot_tenant_link::Link) -> World {
    let store = MemoryLinkStore::new();
    let link = store.insert(link).unwrap();
    World {
        store,
        audit: MemoryAuditLog::new(),
        nonces: MemoryNonceLog::new(),
        id: link.id,
    }
}

impl World {
    fn receive(&self, message: &RevokeMessage, at: u64) -> spot_tenant_link::Result<()> {
        LinkService::new(&self.store, &self.audit)
            .receive_revoke(message, &self.nonces, at)
            .map(|_| ())
    }

    fn state(&self) -> LinkState {
        self.store.get(&self.id).unwrap().state
    }

    fn version(&self) -> u64 {
        self.store.get(&self.id).unwrap().version
    }
}

#[test]
fn a_local_revoke_records_the_local_side_whatever_the_actor_text_says() {
    let w = world();
    let service = LinkService::new(&w.store, &w.audit);
    let revoked = service
        .revoke(&w.id, RevokeReason::TermsEnded, "user:peer-admin", 3_000)
        .unwrap();
    let revocation = revoked.revocation.unwrap();
    assert_eq!(revocation.by, tenant(1), "the local side, never the peer");
    assert_eq!(revocation.at, 3_000);
}

#[test]
fn a_genuine_remote_revoke_ends_the_link_at_once_and_names_the_peer() {
    let w = world();
    let message = RevokeMessage::sign(
        &peer_signer(),
        w.id,
        RevokeReason::SecurityConcern,
        nonce(50),
    );
    assert_eq!(message.version, REVOKE_VERSION);
    assert_eq!(message.revision, REVOKE_ACTIVE_REVISION);
    w.receive(&message, 3_000).unwrap();
    assert_eq!(w.state(), LinkState::Revoked);
    let link = w.store.get(&w.id).unwrap();
    let revocation = link.revocation.unwrap();
    assert_eq!(revocation.by, tenant(2));
    assert_eq!(revocation.reason, RevokeReason::SecurityConcern);
    // The very next use is refused.
    let service = LinkService::new(&w.store, &w.audit);
    assert!(matches!(
        service.authorize(&w.id, &topic(), Direction::Send, 3_001, "user:a", None),
        Err(Error::Revoked { .. })
    ));
    let kinds: Vec<_> = w.audit.events().unwrap().iter().map(|e| e.kind).collect();
    assert_eq!(kinds[0], LinkEventKind::Revoked);
    // Its nonce is spent.
    assert!(!w.nonces.record(&w.id, &nonce(50)).unwrap());
}

#[test]
fn a_forged_remote_revoke_changes_nothing_and_spends_no_nonce() {
    let w = world();
    let genuine = RevokeMessage::sign(&peer_signer(), w.id, RevokeReason::TermsEnded, nonce(51));

    // Unsigned: the signature bytes are zero.
    let mut unsigned = genuine.clone();
    unsigned.signature = spot_tenant_link::SignatureBytes::from_bytes([0u8; 64]);
    // A byte of the signed content changed after signing.
    let mut tampered = genuine.clone();
    tampered.reason = RevokeReason::Superseded;
    // Signed by a key that is not the pinned peer key.
    let stranger = RevokeMessage::sign(
        &stranger_signer(),
        w.id,
        RevokeReason::TermsEnded,
        nonce(52),
    );
    // Signed by the local side's own key (the local side cannot "receive" its own revoke).
    let local = RevokeMessage::sign(
        &SeedSigner::from_seed([1u8; 32], tenant(1)),
        w.id,
        RevokeReason::TermsEnded,
        nonce(53),
    );
    // Valid for another link; this store does not hold it.
    let other = RevokeMessage::sign(
        &peer_signer(),
        link_id(77),
        RevokeReason::TermsEnded,
        nonce(54),
    );
    // Re-pointed at this link after signing for another one.
    let mut repointed = other.clone();
    repointed.link = w.id;
    // Signed for a link still in handshake (revision 3): stale.
    let mut stale = genuine.clone();
    stale.revision = 3;
    stale.nonce = nonce(55);
    stale.signature = peer_signer().sign(&stale.signing_bytes());
    // Signed for a state that does not exist yet.
    let mut future = genuine.clone();
    future.revision = REVOKE_ACTIVE_REVISION + 1;
    future.nonce = nonce(56);
    future.signature = peer_signer().sign(&future.signing_bytes());
    // An encoding version this crate does not know.
    let mut unknown = genuine.clone();
    unknown.version = 2;
    unknown.signature = peer_signer().sign(&unknown.signing_bytes());

    let expectations: [(&RevokeMessage, &str); 8] = [
        (&unsigned, "tenant_link.bad_signature"),
        (&tampered, "tenant_link.bad_signature"),
        (&stranger, "tenant_link.wrong_signer"),
        (&local, "tenant_link.wrong_signer"),
        (&other, "tenant_link.not_found"),
        (&repointed, "tenant_link.bad_signature"),
        (&stale, "tenant_link.out_of_order"),
        (&future, "tenant_link.out_of_order"),
    ];
    for (message, code) in expectations {
        let error = w.receive(message, 3_000).unwrap_err();
        assert_eq!(error.code(), code, "{error}");
        let hex: String = message
            .nonce
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert!(!error.to_string().contains(&hex), "no nonce in an error");
    }
    assert!(matches!(
        w.receive(&unknown, 3_000).unwrap_err(),
        Error::WrongStep { .. }
    ));
    assert_eq!(w.state(), LinkState::Active);
    assert_eq!(w.version(), 1, "nothing was written");
    for seed in [51u8, 52, 53, 54, 55, 56] {
        assert!(
            !w.nonces.contains(&w.id, &nonce(seed)).unwrap(),
            "a refused message spent nonce {seed}"
        );
    }
    // The genuine one still works afterwards.
    w.receive(&genuine, 3_001).unwrap();
    assert_eq!(w.state(), LinkState::Revoked);
}

#[test]
fn a_replayed_remote_revoke_is_refused_and_changes_nothing() {
    let w = world();
    let message = RevokeMessage::sign(&peer_signer(), w.id, RevokeReason::TermsEnded, nonce(60));
    // The nonce was already used on this link: refused as a replay, the link untouched.
    assert!(w.nonces.record(&w.id, &nonce(60)).unwrap());
    let replay = w.receive(&message, 3_000).unwrap_err();
    assert_eq!(replay.code(), "tenant_link.replayed", "{replay}");
    assert_eq!(w.state(), LinkState::Active);
    assert_eq!(w.version(), 1);
    // And a second copy of an applied revoke finds the link already ended.
    let fresh = RevokeMessage::sign(&peer_signer(), w.id, RevokeReason::TermsEnded, nonce(61));
    w.receive(&fresh, 3_001).unwrap();
    let again = w.receive(&fresh, 3_002).unwrap_err();
    assert!(matches!(again, Error::NotActive { .. }), "{again:?}");
    assert_eq!(w.version(), 2);
}

#[test]
fn a_remote_revoke_for_a_link_still_in_handshake_is_refused() {
    let mut link = linked().a;
    link.state = LinkState::AcceptedByPeer;
    let w = world_with(link);
    let message = RevokeMessage::sign(&peer_signer(), w.id, RevokeReason::TermsEnded, nonce(62));
    let error = w.receive(&message, 3_000).unwrap_err();
    assert!(matches!(error, Error::NotActive { .. }), "{error:?}");
    assert_eq!(w.state(), LinkState::AcceptedByPeer);
}

#[test]
fn only_a_message_from_the_pinned_peer_can_cause_an_audit_row() {
    let w = world();
    let mut unsigned =
        RevokeMessage::sign(&peer_signer(), w.id, RevokeReason::TermsEnded, nonce(70));
    unsigned.signature = spot_tenant_link::SignatureBytes::from_bytes([0u8; 64]);
    assert!(w.receive(&unsigned, 3_000).is_err());
    // A validly signed message from a stranger who knows the link id: no row, no nonce.
    let stranger = RevokeMessage::sign(
        &stranger_signer(),
        w.id,
        RevokeReason::TermsEnded,
        nonce(71),
    );
    let error = w.receive(&stranger, 3_000).unwrap_err();
    assert!(matches!(error, Error::WrongSigner { .. }), "{error:?}");
    assert!(
        w.audit.events().unwrap().is_empty(),
        "no row for a stranger"
    );
    assert!(!w.nonces.contains(&w.id, &nonce(71)).unwrap());
    // The pinned peer's own refused message is audited.
    let mut stale = RevokeMessage::sign(&peer_signer(), w.id, RevokeReason::TermsEnded, nonce(72));
    stale.revision = 3;
    stale.signature = peer_signer().sign(&stale.signing_bytes());
    assert!(w.receive(&stale, 3_000).is_err());
    let events = w.audit.events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, LinkEventKind::Refused);
}

/// A store that loses a version race on its first `update` (another writer commits between the
/// read and the write) or fails every `update` while `fail` is set.
struct Flaky {
    inner: MemoryLinkStore,
    conflict_once: std::sync::atomic::AtomicBool,
    fail: std::sync::atomic::AtomicBool,
    updates: std::sync::atomic::AtomicU32,
}

impl Flaky {
    fn new(link: spot_tenant_link::Link) -> Self {
        let inner = MemoryLinkStore::new();
        inner.insert(link).unwrap();
        Self {
            inner,
            conflict_once: false.into(),
            fail: false.into(),
            updates: 0.into(),
        }
    }
}

impl LinkStore for Flaky {
    fn insert(
        &self,
        link: spot_tenant_link::Link,
    ) -> spot_tenant_link::Result<spot_tenant_link::Link> {
        self.inner.insert(link)
    }
    fn get(
        &self,
        id: &spot_tenant_link::LinkId,
    ) -> spot_tenant_link::Result<spot_tenant_link::Link> {
        self.inner.get(id)
    }
    fn update(
        &self,
        link: spot_tenant_link::Link,
        expected: u64,
    ) -> spot_tenant_link::Result<spot_tenant_link::Link> {
        use std::sync::atomic::Ordering::SeqCst;
        self.updates.fetch_add(1, SeqCst);
        if self.fail.load(SeqCst) {
            return Err(Error::StoreFailed {
                reason: "the test store is down",
            });
        }
        if self.conflict_once.swap(false, SeqCst) {
            // A concurrent local write that leaves the link as it was but moves its version.
            let current = self.inner.get(&link.id)?;
            self.inner.update(current.clone(), current.version)?;
        }
        self.inner.update(link, expected)
    }
    fn find(
        &self,
        first: &spot_tenant_link::Party,
        second: &spot_tenant_link::Party,
    ) -> spot_tenant_link::Result<Option<spot_tenant_link::Link>> {
        self.inner.find(first, second)
    }
    fn list_for(
        &self,
        party: &spot_tenant_link::Party,
    ) -> spot_tenant_link::Result<Vec<spot_tenant_link::Link>> {
        self.inner.list_for(party)
    }
}

#[test]
fn a_revoke_that_loses_a_version_race_still_applies_and_records_its_nonce_once() {
    use std::sync::atomic::Ordering::SeqCst;
    let store = Flaky::new(linked().a);
    let id = store.inner.list_for(&tenant(1)).unwrap()[0].id;
    store.conflict_once.store(true, SeqCst);
    let audit = MemoryAuditLog::new();
    let nonces = MemoryNonceLog::new();
    let message = RevokeMessage::sign(&peer_signer(), id, RevokeReason::SecurityConcern, nonce(80));
    let revoked = LinkService::new(&store, &audit)
        .receive_revoke(&message, &nonces, 3_000)
        .unwrap();
    assert_eq!(revoked.state, LinkState::Revoked);
    assert_eq!(revoked.revocation.unwrap().by, tenant(2));
    assert_eq!(store.updates.load(SeqCst), 2, "one conflict, one retry");
    assert!(nonces.contains(&id, &nonce(80)).unwrap());
    // Recorded once: a second claim finds it.
    assert!(!nonces.record(&id, &nonce(80)).unwrap());
}

#[test]
fn a_revoke_whose_commit_fails_does_not_burn_its_nonce_and_a_resend_works() {
    use std::sync::atomic::Ordering::SeqCst;
    let store = Flaky::new(linked().a);
    let id = store.inner.list_for(&tenant(1)).unwrap()[0].id;
    store.fail.store(true, SeqCst);
    let audit = MemoryAuditLog::new();
    let nonces = MemoryNonceLog::new();
    let service = LinkService::new(&store, &audit);
    let message = RevokeMessage::sign(&peer_signer(), id, RevokeReason::SecurityConcern, nonce(81));
    let error = service
        .receive_revoke(&message, &nonces, 3_000)
        .unwrap_err();
    assert!(matches!(error, Error::StoreFailed { .. }), "{error:?}");
    assert!(
        !nonces.contains(&id, &nonce(81)).unwrap(),
        "the nonce is not burned"
    );
    assert_eq!(store.inner.get(&id).unwrap().state, LinkState::Active);
    // The store recovers; the same signed message is accepted.
    store.fail.store(false, SeqCst);
    let revoked = service.receive_revoke(&message, &nonces, 3_001).unwrap();
    assert_eq!(revoked.state, LinkState::Revoked);
    assert!(nonces.contains(&id, &nonce(81)).unwrap());
}
