//! The three trust-boundary fixes on top of WP-CTL-09.T1:
//!
//! - the peer identity key is pinned from a trusted directory, never taken from the first
//!   message; a first use needs an administrator's confirmed fingerprint;
//! - every consent is re-checked when it is used (shape, age and the step-up gate), in `propose`,
//!   in `from_proposal` and in `accept`;
//! - used nonces live in a durable log, so a handshake rebuilt from the stored link cannot be
//!   made to accept a captured message a second time.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Ports, consent, link_id, nonce, offer, proof, tenant};
use spot_iam_access::{AuthMethod, OperationName};
use spot_tenant_link::{
    CONSENT_OPERATION, Carrier, Consent, ConsentPolicy, Direction, Error, Handshake, HandshakeBody,
    IdentitySigner, LinkDraft, LinkEventKind, MemoryAuditLog, Party, Received, Resume, SeedSigner,
    StepUpProof, codes,
};

/// The one-grant draft every test starts from.
fn draft(a: &Party, b: &Party) -> LinkDraft {
    LinkDraft::new(
        link_id(1),
        a.clone(),
        b.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer(a, Direction::Send),
    )
    .unwrap()
}

/// Unwraps a refusal; the success type of a handshake constructor is not `Debug`.
fn expect_err<T>(result: Result<T, Error>) -> Error {
    match result {
        Ok(_) => panic!("expected a refusal"),
        Err(error) => error,
    }
}

/// Whether `audit` holds a refusal carrying `code`.
fn audited(audit: &MemoryAuditLog, code: &str) -> bool {
    audit
        .events()
        .unwrap()
        .iter()
        .any(|event| event.kind == LinkEventKind::Refused && event.detail == Some(code))
}

/// A consent dated `at` with a strong sign-in ten seconds earlier.
fn strong(party: &Party, at: u64) -> Consent {
    Consent {
        party: party.clone(),
        actor: "user:admin".to_owned(),
        at,
        proof: proof(at),
    }
}

/// A consent recorded long ago: its step-up proof was strong then, not now.
fn stale(party: &Party, at: u64) -> Consent {
    strong(party, at)
}

/// A consent resting on a password alone.
fn password_only(party: &Party, at: u64) -> Consent {
    Consent {
        party: party.clone(),
        actor: "user:admin".to_owned(),
        at,
        proof: StepUpProof {
            operation: OperationName::new(CONSENT_OPERATION),
            method: AuthMethod::Password,
            authenticated_at: i64::try_from(at / 1_000).unwrap() - 10,
        },
    }
}

/// A consent whose proof names a different operation.
fn other_operation(party: &Party, at: u64) -> Consent {
    Consent {
        party: party.clone(),
        actor: "user:admin".to_owned(),
        at,
        proof: StepUpProof {
            operation: OperationName::new("document_publish"),
            method: AuthMethod::PasswordTotp,
            authenticated_at: i64::try_from(at / 1_000).unwrap() - 10,
        },
    }
}

/// A consent whose actor is the system rather than an administrator.
fn by_system(party: &Party, at: u64) -> Consent {
    Consent {
        party: party.clone(),
        actor: "system".to_owned(),
        at,
        proof: proof(at),
    }
}

/// Runs one bad consent through `propose`, `from_proposal` and `accept` and proves each entry
/// point refuses it with `code` and writes the refusal to its own trail.
fn refused_in_all_three(
    bad: &dyn Fn(&Party, u64) -> Consent,
    consent_at: u64,
    use_at: u64,
    max_age_ms: u64,
    code: &'static str,
) {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());

    // The proposal every other entry point starts from: valid when it was built, so a refusal
    // later names the substituted consent rather than the handshake setup.
    let setup_audit = MemoryAuditLog::new();
    let setup_ports = Ports::pinned(&b, key_b.public_key());
    let setup_policy = setup_ports.policy();
    let (_handshake, good) = Handshake::propose(
        &key_a,
        &setup_audit,
        &setup_policy,
        draft(&a, &b),
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let mut tampered = good.clone();
    if let HandshakeBody::Propose(body) = &mut tampered.body {
        body.consent = bad(&a, consent_at);
    }
    tampered.signature = key_a.sign(&tampered.signing_bytes());

    // 1. propose
    let audit_a = MemoryAuditLog::new();
    let mut ports_a = Ports::pinned(&b, key_b.public_key());
    ports_a.consent = ConsentPolicy::new(max_age_ms);
    let policy_a = ports_a.policy();
    let error = expect_err(Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft(&a, &b),
        bad(&a, consent_at),
        use_at,
        nonce(2),
    ));
    assert_eq!(error.code(), code, "propose: {error}");
    assert!(audited(&audit_a, code), "propose did not audit {code}");

    // 2. from_proposal
    let audit_b = MemoryAuditLog::new();
    let mut ports_b = Ports::pinned(&a, key_a.public_key());
    ports_b.consent = ConsentPolicy::new(max_age_ms);
    let policy_b = ports_b.policy();
    let error = expect_err(Handshake::from_proposal(
        &key_b, &audit_b, &policy_b, &tampered, use_at,
    ));
    assert_eq!(error.code(), code, "from_proposal: {error}");
    assert!(
        audited(&audit_b, code),
        "from_proposal did not audit {code}"
    );

    // 3. accept
    let accept_audit = MemoryAuditLog::new();
    let mut accept_ports = Ports::pinned(&a, key_a.public_key());
    accept_ports.consent = ConsentPolicy::new(max_age_ms);
    let accept_policy = accept_ports.policy();
    let mut peer =
        Handshake::from_proposal(&key_b, &accept_audit, &accept_policy, &good, 1_200).unwrap();
    let error = expect_err(peer.accept(
        offer(&b, Direction::Receive),
        bad(&b, consent_at),
        use_at,
        nonce(3),
    ));
    assert_eq!(error.code(), code, "accept: {error}");
    assert!(audited(&accept_audit, code), "accept did not audit {code}");
}

#[test]
fn a_stale_consent_is_refused_in_all_three_entry_points() {
    // The consent was recorded at 1_000; it is used at 62_000, past a one-minute policy.
    refused_in_all_three(&stale, 1_000, 62_000, 60_000, codes::CONSENT_STALE);
}

#[test]
fn a_password_only_consent_is_refused_in_all_three_entry_points() {
    refused_in_all_three(
        &password_only,
        1_100,
        1_300,
        ConsentPolicy::DEFAULT_MAX_AGE_MS,
        codes::STEP_UP_REQUIRED,
    );
}

#[test]
fn a_consent_for_another_operation_is_refused_in_all_three_entry_points() {
    refused_in_all_three(
        &other_operation,
        1_100,
        1_300,
        ConsentPolicy::DEFAULT_MAX_AGE_MS,
        codes::CONSENT_INVALID,
    );
}

#[test]
fn a_system_consent_is_refused_in_all_three_entry_points() {
    refused_in_all_three(
        &by_system,
        1_100,
        1_300,
        ConsentPolicy::DEFAULT_MAX_AGE_MS,
        codes::CONSENT_INVALID,
    );
}

#[test]
fn a_proposal_signed_with_a_key_that_differs_from_the_pin_is_refused_and_audited() {
    let a = tenant(1);
    let b = tenant(2);
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let real_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let impostor = SeedSigner::from_seed([9u8; 32], a.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let policy_a = ports_a.policy();
    let (_handshake, proposal) = Handshake::propose(
        &impostor,
        &audit_a,
        &policy_a,
        draft(&a, &b),
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();

    // The invitee knows A's real key; the message presents another one.
    let ports_b = Ports::pinned(&a, real_a.public_key());
    let policy_b = ports_b.policy();
    let error = expect_err(Handshake::from_proposal(
        &key_b, &audit_b, &policy_b, &proposal, 1_200,
    ));
    assert_eq!(error.code(), codes::KEY_MISMATCH);
    assert!(audited(&audit_b, codes::KEY_MISMATCH));
}

#[test]
fn an_acceptance_signed_with_another_key_is_refused_by_the_proposer() {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let real_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let impostor_b = SeedSigner::from_seed([8u8; 32], b.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();

    let ports_a = Ports::pinned(&b, real_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let policy_a = ports_a.policy();
    let policy_b = ports_b.policy();
    let (mut proposer, proposal) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft(&a, &b),
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let mut peer =
        Handshake::from_proposal(&impostor_b, &audit_b, &policy_b, &proposal, 1_200).unwrap();
    let accept = peer
        .accept(
            offer(&b, Direction::Receive),
            consent(&b, "user:b", 1_300),
            1_300,
            nonce(2),
        )
        .unwrap();

    let error = expect_err(proposer.receive(&accept, 1_400));
    assert_eq!(error.code(), codes::KEY_MISMATCH);
    assert!(audited(&audit_a, codes::KEY_MISMATCH));
}

#[test]
fn a_party_with_no_pinned_key_is_refused_without_a_first_use_confirmation() {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let audit_a = MemoryAuditLog::new();
    let audit_b = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let policy_a = ports_a.policy();
    let (_handshake, proposal) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft(&a, &b),
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();

    // Nothing is pinned and the caller confirmed no fingerprint.
    let ports_b = Ports::empty();
    let policy_b = ports_b.policy();
    let error = expect_err(Handshake::from_proposal(
        &key_b, &audit_b, &policy_b, &proposal, 1_200,
    ));
    assert_eq!(error.code(), codes::KEY_UNPINNED);
    assert!(audited(&audit_b, codes::KEY_UNPINNED));
}

#[test]
fn a_first_use_confirmation_must_match_the_key_in_the_message() {
    let a = tenant(1);
    let b = tenant(2);
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let real_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let impostor = SeedSigner::from_seed([9u8; 32], a.clone());
    let audit_a = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let policy_a = ports_a.policy();
    let (_handshake, proposal) = Handshake::propose(
        &impostor,
        &audit_a,
        &policy_a,
        draft(&a, &b),
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();

    // The administrator confirmed the real key's fingerprint, not the one in the message.
    let audit_b = MemoryAuditLog::new();
    let ports_b = Ports::first_use(real_a.public_key());
    let policy_b = ports_b.policy();
    let error = expect_err(Handshake::from_proposal(
        &key_b, &audit_b, &policy_b, &proposal, 1_200,
    ));
    assert_eq!(error.code(), codes::KEY_MISMATCH);
    assert!(audited(&audit_b, codes::KEY_MISMATCH));

    // The same message, signed by the key the administrator confirmed, is accepted.
    let audit_c = MemoryAuditLog::new();
    let ports_c = Ports::first_use(real_a.public_key());
    let policy_c = ports_c.policy();
    let (_handshake, confirmed) = Handshake::propose(
        &real_a,
        &audit_a,
        &policy_a,
        draft(&a, &b),
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(4),
    )
    .unwrap();
    assert!(
        Handshake::from_proposal(&key_b, &audit_c, &policy_c, &confirmed, 1_200).is_ok(),
        "a first-use key the administrator confirmed is accepted"
    );
}

#[test]
fn a_replayed_acceptance_is_refused_after_a_restart_with_the_same_nonce_log() {
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
    let (mut proposer, proposal) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft(&a, &b),
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    // The link as it would be stored before the acceptance lands.
    let stored = proposer.link().clone();
    let mut peer = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &proposal, 1_200).unwrap();
    let accept = peer
        .accept(
            offer(&b, Direction::Receive),
            consent(&b, "user:b", 1_300),
            1_300,
            nonce(2),
        )
        .unwrap();
    proposer.receive(&accept, 1_400).unwrap();

    // A restart rebuilds the handshake from the stored link and the transcript tail. Its
    // in-memory nonce set is empty, but the log still holds the acceptance's nonce.
    let mut resumed = Handshake::resume(
        &key_a,
        &audit_a,
        &policy_a,
        Resume {
            link: stored,
            local_offer: offer(&a, Direction::Send),
            remote_offer: None,
            head: proposal.digest(),
            next_revision: 1,
        },
    )
    .unwrap();
    let error = expect_err(resumed.receive(&accept, 1_500));
    assert_eq!(error.code(), codes::REPLAYED);
    assert!(audited(&audit_a, codes::REPLAYED));
}

#[test]
fn a_fresh_nonce_log_cannot_recognise_the_replay_which_is_why_it_must_be_durable() {
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
    let (mut proposer, proposal) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft(&a, &b),
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let stored = proposer.link().clone();
    let mut peer = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &proposal, 1_200).unwrap();
    let accept = peer
        .accept(
            offer(&b, Direction::Receive),
            consent(&b, "user:b", 1_300),
            1_300,
            nonce(2),
        )
        .unwrap();
    proposer.receive(&accept, 1_400).unwrap();

    // A rebuilt handshake that came up with a new, empty log has forgotten the nonce and accepts
    // the captured acceptance. This is exactly why the NonceLog must be as durable as the link.
    let lost_ports = Ports::pinned(&b, key_b.public_key());
    let lost_policy = lost_ports.policy();
    let mut resumed = Handshake::resume(
        &key_a,
        &audit_a,
        &lost_policy,
        Resume {
            link: stored,
            local_offer: offer(&a, Direction::Send),
            remote_offer: None,
            head: proposal.digest(),
            next_revision: 1,
        },
    )
    .unwrap();
    assert_eq!(
        resumed.receive(&accept, 1_500).unwrap(),
        Received::Accepted,
        "a lost nonce log re-applies the message; the log must be durable"
    );
}

#[test]
fn a_send_that_reuses_a_recorded_nonce_is_refused() {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let audit_a = MemoryAuditLog::new();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let policy_a = ports_a.policy();
    Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft(&a, &b),
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();

    // A retry after a restart reuses the first message's nonce; the log refuses it.
    let error = expect_err(Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft(&a, &b),
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    ));
    assert_eq!(error.code(), codes::REPLAYED);
    assert!(audited(&audit_a, codes::REPLAYED));
}
