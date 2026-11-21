//! Shared fixtures for the integration tests: two parties, a message topic, a grant, a consent,
//! the ports a handshake needs, and one complete handshake.
//!
//! Every instant is fixed, so the tests are exact and read no clock.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use spot_iam_access::{AuthMethod, OperationName, StepUpPolicy};
use spot_tenancy::TenantId;
use spot_tenant_link::{
    CONSENT_OPERATION, Carrier, Consent, ConsentPolicy, Direction, FieldList, FieldName, Grant,
    Handshake, HandshakePolicy, IdentitySigner, Link, LinkDraft, LinkId, MemoryAuditLog,
    MemoryNonceLog, MemoryPeerKeyDirectory, Nonce, Offer, Party, PeerKey, PolicyStepUp,
    SchemaRange, SeedSigner, StepUpProof, Topic, TopicKind, Trust,
};

/// A tenant party with a deterministic UUIDv7.
pub fn tenant(n: u8) -> Party {
    Party::Tenant(TenantId::new_v7(1_700_000_000_000, [n; 10]))
}

/// A fixed id, different per seed.
pub fn link_id(seed: u8) -> LinkId {
    LinkId::new_v7(1_700_000_000_000, [seed; 10])
}

/// The exchange topic every fixture uses.
pub fn topic() -> Topic {
    Topic::new("bpm.message").unwrap()
}

/// A nonce, different per seed.
pub fn nonce(seed: u8) -> Nonce {
    Nonce::from_bytes([seed; 16])
}

/// A message grant on [`topic`] with one listed field.
pub fn grant(direction: Direction) -> Grant {
    Grant {
        topic: topic(),
        kind: TopicKind::Message,
        direction,
        schema_range: SchemaRange::new(1, 2).unwrap(),
        fields: FieldList::only([FieldName::new("message.body").unwrap()]),
        expires: 100_000,
        require_peer_region: None,
        notify: true,
    }
}

/// An offer of one grant.
pub fn offer(side: &Party, direction: Direction) -> Offer {
    Offer::new(side.clone(), vec![grant(direction)]).unwrap()
}

/// The step-up gate over the default tenant policy.
pub fn gate() -> PolicyStepUp {
    PolicyStepUp::new(StepUpPolicy::default())
}

/// A consent at `at`, with a strong sign-in ten seconds earlier.
pub fn consent(party: &Party, actor: &str, at: u64) -> Consent {
    Consent::new(
        party.clone(),
        actor,
        at,
        StepUpProof {
            operation: OperationName::new(CONSENT_OPERATION),
            method: AuthMethod::PasswordTotp,
            authenticated_at: i64::try_from(at / 1_000).unwrap() - 10,
        },
        &gate(),
    )
    .unwrap()
}

/// The step-up proof a consent carries, ten seconds before `at`.
pub fn proof(at: u64) -> StepUpProof {
    StepUpProof {
        operation: OperationName::new(CONSENT_OPERATION),
        method: AuthMethod::PasswordTotp,
        authenticated_at: i64::try_from(at / 1_000).unwrap() - 10,
    }
}

/// One side's handshake ports: a peer key directory, a durable nonce log, a step-up gate and the
/// trust it places in the other side's first key.
pub struct Ports {
    /// The keys this side already trusts.
    pub keys: MemoryPeerKeyDirectory,
    /// The durable nonce record.
    pub log: MemoryNonceLog,
    /// The tenant's step-up gate.
    pub gate: PolicyStepUp,
    /// How a key with no pin may be trusted.
    pub trust: Trust,
    /// How old a used consent may be.
    pub consent: ConsentPolicy,
}

impl Ports {
    /// Ports that already pin `peer`'s identity key.
    pub fn pinned(peer: &Party, key: PeerKey) -> Self {
        Self {
            keys: MemoryPeerKeyDirectory::with(peer.clone(), key).unwrap(),
            log: MemoryNonceLog::new(),
            gate: gate(),
            trust: Trust::PinnedOnly,
            consent: ConsentPolicy::default(),
        }
    }

    /// Ports with an empty directory that accept only a pinned key.
    pub fn empty() -> Self {
        Self {
            keys: MemoryPeerKeyDirectory::new(),
            log: MemoryNonceLog::new(),
            gate: gate(),
            trust: Trust::PinnedOnly,
            consent: ConsentPolicy::default(),
        }
    }

    /// Ports with an empty directory that confirm `key` on first use.
    pub fn first_use(key: PeerKey) -> Self {
        Self {
            trust: Trust::first_use(key),
            ..Self::empty()
        }
    }

    /// The policy borrowing this fixture, for one handshake constructor.
    pub fn policy(&self) -> HandshakePolicy<'_> {
        HandshakePolicy {
            keys: &self.keys,
            trust: self.trust.clone(),
            consent: self.consent,
            gate: &self.gate,
            nonces: &self.log,
        }
    }
}

/// The two links and both audit logs of one complete handshake.
pub struct Linked {
    /// The proposer's copy.
    pub a: Link,
    /// The peer's copy.
    pub b: Link,
    /// The proposer's local trail.
    pub audit_a: MemoryAuditLog,
    /// The peer's local trail.
    pub audit_b: MemoryAuditLog,
}

/// Runs a complete direct handshake between two tenants, A sending and B receiving.
pub fn linked() -> Linked {
    linked_with(Carrier::Direct, Direction::Send, Direction::Receive)
}

/// Runs a complete handshake with the given carrier and directions.
pub fn linked_with(carrier: Carrier, direction_a: Direction, direction_b: Direction) -> Linked {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let policy_a = ports_a.policy();
    let policy_b = ports_b.policy();
    let draft = LinkDraft::new(
        link_id(1),
        a.clone(),
        b.clone(),
        carrier,
        1_000,
        None,
        offer(&a, direction_a),
    )
    .unwrap();
    let (mut proposer, propose) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft,
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let mut peer = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &propose, 1_200).unwrap();
    let accept = peer
        .accept(
            offer(&b, direction_b),
            consent(&b, "user:b", 1_300),
            1_300,
            nonce(2),
        )
        .unwrap();
    proposer.receive(&accept, 1_400).unwrap();
    let key_message = proposer.key_exchange(1_450, nonce(3)).unwrap();
    peer.receive(&key_message, 1_500).unwrap();
    let key_back = peer.key_exchange(1_550, nonce(4)).unwrap();
    proposer.receive(&key_back, 1_600).unwrap();
    let activate = proposer.activate(1_700, nonce(5)).unwrap();
    peer.receive(&activate, 1_800).unwrap();
    Linked {
        a: proposer.into_link(),
        b: peer.into_link(),
        audit_a,
        audit_b,
    }
}
