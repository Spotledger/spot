//! The refusal matrix: every way a command must be refused.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Harness, key};
use ed25519_dalek::Signature;
use futures::executor::block_on;
use spot_agent_core::{
    Agent, Decision, DelayOnly, EnvelopeSignature, MemoryResultLog, NoCritical, Policy, Refusal,
    TrustStore,
};
use spot_agent_enroll::keys::Protection;
use spot_mgmt_ops::Risk;
use spot_signed_command::MemoryNonceStore;
use std::sync::Arc;

fn decide(
    harness: &Harness,
    agent: &Agent,
    op: &str,
    signers: &[&ed25519_dalek::SigningKey],
    now: i64,
) -> Decision {
    let command = harness.command(op, common::valid_params(op), now - 30, now + 600);
    let envelope = harness.envelope(&command, signers);
    block_on(agent.handle(&envelope, now))
}

#[test]
fn unsigned_command_is_refused() {
    let harness = Harness::new();
    let agent = harness.agent();
    let decision = decide(&harness, &agent, "server.service.restart", &[], 1_000);
    assert_eq!(
        decision.refusal(),
        Some(&Refusal::InsufficientSignatures { have: 0, need: 1 })
    );
}

#[test]
fn forged_signature_is_refused() {
    let harness = Harness::new();
    let agent = harness.agent();
    let command = harness.command("server.info.read", serde_json::json!({}), 970, 1_600);
    // A signature that is not over the command.
    let forged = EnvelopeSignature::new(
        harness.admin_a.verifying_key().to_bytes(),
        Signature::from_bytes(&[0u8; 64]),
    )
    .unwrap();
    let envelope = spot_agent_core::CommandEnvelope::from_parts(
        String::from_utf8(command.canonical_bytes().unwrap()).unwrap(),
        vec![forged],
    )
    .unwrap();
    assert_eq!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(&Refusal::InvalidSignature)
    );
}

#[test]
fn an_unknown_signer_is_refused() {
    let harness = Harness::new();
    let agent = harness.agent();
    let stranger = key(99);
    assert_eq!(
        decide(&harness, &agent, "server.info.read", &[&stranger], 1_000).refusal(),
        Some(&Refusal::UnknownSigner)
    );
}

#[test]
fn wrong_audience_is_refused() {
    let harness = Harness::new();
    let agent = harness.agent();
    let command = harness.command_for("server.info.read", "another-agent", 970, 1_600);
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    assert_eq!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(&Refusal::WrongAudience)
    );
}

#[test]
fn expired_and_future_commands_are_refused() {
    let harness = Harness::new();
    let agent = harness.agent();
    let command = harness.command("server.info.read", serde_json::json!({}), 500, 1_000);
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    assert_eq!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(&Refusal::Expired)
    );
    let command = harness.command("server.info.read", serde_json::json!({}), 2_000, 2_600);
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    assert_eq!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(&Refusal::NotYetValid)
    );
}

#[test]
fn a_replay_is_refused() {
    let harness = Harness::new();
    let agent = harness.agent();
    let command = harness.command("server.info.read", serde_json::json!({}), 970, 1_600);
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    assert!(matches!(
        block_on(agent.handle(&envelope, 1_000)),
        Decision::Execute(_)
    ));
    assert_eq!(
        block_on(agent.handle(&envelope, 1_001)).refusal(),
        Some(&Refusal::Replay)
    );
}

#[test]
fn one_of_two_signatures_is_refused() {
    let harness = Harness::new();
    let agent = harness.agent();
    assert_eq!(
        decide(
            &harness,
            &agent,
            "tenant.backup.run",
            &[&harness.admin_a],
            1_000
        )
        .refusal(),
        Some(&Refusal::InsufficientSignatures { have: 1, need: 2 })
    );
}

#[test]
fn the_same_signer_twice_counts_once() {
    let harness = Harness::new();
    let agent = harness.agent();
    let command = harness.command(
        "tenant.backup.run",
        common::valid_params("tenant.backup.run"),
        970,
        1_600,
    );
    let envelope = harness.envelope(&command, &[&harness.admin_a, &harness.admin_a]);
    assert_eq!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(&Refusal::InsufficientSignatures { have: 1, need: 2 })
    );
}

#[test]
fn the_requester_may_not_approve_a_dangerous_command() {
    let harness = Harness::new();
    let agent = harness.agent();
    // The requester signs too; two distinct valid keys, but one is the requester.
    assert_eq!(
        decide(
            &harness,
            &agent,
            "tenant.backup.run",
            &[&harness.requester, &harness.admin_b],
            1_000,
        )
        .refusal(),
        Some(&Refusal::RequesterCannotApprove {
            requester: common::key_id(&harness.requester).as_str().to_owned()
        })
    );
}

#[test]
fn a_software_key_is_refused_for_a_dangerous_command() {
    let harness = Harness::new();
    let mut trust = TrustStore::new();
    trust
        .insert(
            &harness.admin_a.verifying_key().to_bytes(),
            Protection::Software,
        )
        .unwrap();
    trust
        .insert(
            &harness.admin_b.verifying_key().to_bytes(),
            Protection::Software,
        )
        .unwrap();
    let agent = Agent::new(
        "agent-1",
        harness.catalogue.clone(),
        trust,
        Policy::default(),
        Arc::new(MemoryNonceStore::new()),
        Box::new(DelayOnly::new(0)),
        Arc::new(MemoryResultLog::new("agent-1")),
    );
    assert_eq!(
        decide(
            &harness,
            &agent,
            "tenant.backup.run",
            &[&harness.admin_a, &harness.admin_b],
            1_000
        )
        .refusal(),
        Some(&Refusal::SoftwareKeyRefused {
            key_id: common::key_id(&harness.admin_a).as_str().to_owned()
        })
    );
}

#[test]
fn a_revoked_signer_is_refused() {
    let harness = Harness::new();
    let mut trust = harness.trust.clone();
    trust.revoke(&common::key_id(&harness.admin_a), 900);
    let agent = Agent::new(
        "agent-1",
        harness.catalogue.clone(),
        trust,
        Policy::default(),
        Arc::new(MemoryNonceStore::new()),
        Box::new(DelayOnly::new(0)),
        Arc::new(MemoryResultLog::new("agent-1")),
    );
    assert_eq!(
        decide(
            &harness,
            &agent,
            "tenant.backup.run",
            &[&harness.admin_a, &harness.admin_b],
            1_000
        )
        .refusal(),
        Some(&Refusal::RevokedSigner)
    );
}

#[test]
fn unknown_operation_is_refused() {
    let harness = Harness::new();
    let agent = harness.agent();
    let command = harness.command("no.such.operation", serde_json::json!({}), 970, 1_600);
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    assert_eq!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(&Refusal::UnknownOperation)
    );
}

#[test]
fn argument_mismatches_are_refused() {
    let harness = Harness::new();
    let agent = harness.agent();
    // Missing a required parameter.
    let command = harness.command("tenant.user.create", serde_json::json!({}), 970, 1_600);
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    assert!(matches!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(Refusal::InvalidArguments { .. })
    ));
    // Unknown parameter.
    let command = harness.command(
        "tenant.user.create",
        serde_json::json!({"user_name": "alice", "surprise": 1}),
        971,
        1_601,
    );
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    assert!(matches!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(Refusal::InvalidArguments { .. })
    ));
    // Wrong type.
    let command = harness.command(
        "tenant.user.create",
        serde_json::json!({"user_name": 3}),
        972,
        1_602,
    );
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    assert!(matches!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(Refusal::InvalidArguments { .. })
    ));
}

#[test]
fn a_risk_field_in_the_envelope_cannot_downgrade() {
    let harness = Harness::new();
    let agent = harness.agent();
    // A critical operation whose parameters smuggle a "risk" field: refused as an unknown
    // parameter, so the envelope can never carry a risk level of its own.
    let command = harness.command(
        "system.disk.wipe",
        serde_json::json!({"disk": "/dev/sda", "risk": "read"}),
        970,
        1_600,
    );
    let envelope = harness.envelope(&command, &[&harness.admin_a, &harness.admin_b]);
    assert!(matches!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(Refusal::InvalidArguments { .. })
    ));
    // And a high operation still needs two signatures whatever the parameters claim: the
    // claimed risk can never lower the catalogue's requirement.
    let command = harness.command(
        "tenant.backup.run",
        serde_json::json!({"passphrase": "x", "risk": "read"}),
        971,
        1_601,
    );
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    assert_eq!(
        block_on(agent.handle(&envelope, 1_000)).refusal(),
        Some(&Refusal::InsufficientSignatures { have: 1, need: 2 })
    );
}

#[test]
fn the_risk_level_comes_from_the_catalogue() {
    let harness = Harness::new();
    let agent = harness.agent();
    let decision = decide(
        &harness,
        &agent,
        "tenant.backup.run",
        &[&harness.admin_a, &harness.admin_b],
        1_000,
    );
    let plan = decision.plan().expect("a two-person high operation runs");
    assert_eq!(plan.risk(), Risk::High);
}

#[test]
fn a_critical_operation_needs_clearance() {
    let harness = Harness::new();
    let agent = Agent::new(
        "agent-1",
        harness.catalogue.clone(),
        harness.trust.clone(),
        Policy::default(),
        Arc::new(MemoryNonceStore::new()),
        Box::new(NoCritical),
        Arc::new(MemoryResultLog::new("agent-1")),
    );
    assert_eq!(
        decide(
            &harness,
            &agent,
            "system.disk.wipe",
            &[&harness.admin_a, &harness.admin_b],
            1_000,
        )
        .refusal(),
        Some(&Refusal::CriticalNotCleared {
            op: "system.disk.wipe".to_owned()
        })
    );
}

#[test]
fn a_critical_operation_cleared_by_delay_runs() {
    let harness = Harness::new();
    let agent = harness.agent();
    let decision = decide(
        &harness,
        &agent,
        "system.disk.wipe",
        &[&harness.admin_a, &harness.admin_b],
        1_000,
    );
    assert!(matches!(decision, Decision::Execute(_)));
}

#[test]
fn from_identity_uses_the_enrolled_agent_id_as_audience() {
    let harness = Harness::new();
    let identity = spot_agent_enroll::identity::AgentIdentity {
        agent_id: "agent-1".to_owned(),
        server_public_key: spot_agent_enroll::message::PublicKeyB64::from_bytes_unchecked(
            &[4u8; 32],
        ),
        enrolled_at: 0,
    };
    let agent = Agent::from_identity(
        &identity,
        harness.catalogue.clone(),
        harness.trust.clone(),
        Policy::default(),
        Arc::new(MemoryNonceStore::new()),
        Box::new(DelayOnly::new(0)),
        Arc::new(MemoryResultLog::new("agent-1")),
    );
    assert_eq!(agent.agent_id(), "agent-1");
    let command = harness.command("server.info.read", serde_json::json!({}), 970, 1_600);
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    assert!(matches!(
        block_on(agent.handle(&envelope, 1_000)),
        Decision::Execute(_)
    ));
}

/// A nonce store that cannot answer.
struct BrokenNonces;

impl spot_signed_command::NonceStore for BrokenNonces {
    fn check_and_insert<'a>(
        &'a self,
        _key: &'a spot_signed_command::KeyId,
        _nonce: [u8; 16],
        _expires_at: i64,
        _now: i64,
    ) -> spot_signed_command::NonceFuture<'a, Result<bool, spot_signed_command::StoreError>> {
        Box::pin(std::future::ready(Err(
            spot_signed_command::StoreError::Unavailable,
        )))
    }
}

/// A store that cannot answer refuses a fully valid command; it is never "fresh".
#[test]
fn an_unavailable_nonce_store_fails_closed() {
    let harness = Harness::new();
    let agent = Agent::new(
        harness.agent_id.clone(),
        harness.catalogue.clone(),
        harness.trust.clone(),
        harness.policy.clone(),
        Arc::new(BrokenNonces),
        Box::new(DelayOnly::new(0)),
        harness.log.clone(),
    );
    let decision = decide(
        &harness,
        &agent,
        "server.info.read",
        &[&harness.admin_a],
        1_000,
    );
    assert_eq!(decision.refusal(), Some(&Refusal::NonceStore));
    assert!(decision.plan().is_none());
}
