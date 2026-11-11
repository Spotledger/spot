//! Shared fixtures for the `spot-agent-core` integration tests: a small catalogue with one
//! operation per risk level, a hardware-key trust store, and builders for commands and
//! envelopes.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use ed25519_dalek::{Signer as _, SigningKey};
use spot_agent_core::{
    Agent, CommandEnvelope, DelayOnly, EnvelopeSignature, MemoryResultLog, Policy, TrustStore,
};
use spot_agent_enroll::keys::Protection;
use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, OpId, OperationSpec, ParamSpec, ParamType, ParamValue, Risk,
    TargetKind,
};
use spot_signed_command::{Command, KeyId, MemoryNonceStore};

/// A deterministic Ed25519 key from a one-byte seed.
pub fn key(seed: u8) -> SigningKey {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    SigningKey::from_bytes(&bytes)
}

/// The key id of a signing key.
pub fn key_id(signer: &SigningKey) -> KeyId {
    KeyId::from_public_key(&signer.verifying_key().to_bytes())
}

fn text_param(name: &str, required: bool, secret: bool) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len: 64,
            pattern: None,
        },
        required,
        default: None,
        doc: format!("The {name} parameter."),
        secret,
    }
}

fn op(id: &str, risk: Risk, params: Vec<ParamSpec>) -> OperationSpec {
    OperationSpec {
        id: OpId::new(id).unwrap(),
        title: id.to_owned(),
        summary: format!("Test operation {id}."),
        params,
        risk,
        target: TargetKind::Server,
    }
}

/// The test catalogue: one operation per risk level.
pub fn catalogue() -> Catalogue {
    let mut builder = CatalogueBuilder::new();
    builder.add("test.read", op("server.info.read", Risk::Read, vec![]));
    builder.add(
        "test.low",
        op(
            "tenant.user.create",
            Risk::Low,
            vec![text_param("user_name", true, false)],
        ),
    );
    builder.add(
        "test.medium",
        op(
            "server.service.restart",
            Risk::Medium,
            vec![
                text_param("name", true, false),
                ParamSpec {
                    name: "wait".to_owned(),
                    ty: ParamType::Duration,
                    required: false,
                    default: Some(ParamValue::Duration(30)),
                    doc: "Seconds to wait.".to_owned(),
                    secret: false,
                },
            ],
        ),
    );
    builder.add(
        "test.high",
        op(
            "tenant.backup.run",
            Risk::High,
            vec![text_param("passphrase", true, true)],
        ),
    );
    builder.add(
        "test.critical",
        op(
            "system.disk.wipe",
            Risk::Critical,
            vec![text_param("disk", true, false)],
        ),
    );
    builder.build().unwrap()
}

/// The valid parameters of the test operation `op`, as command JSON.
pub fn valid_params(op_id: &str) -> serde_json::Value {
    match op_id {
        "server.info.read" => serde_json::json!({}),
        "tenant.user.create" => serde_json::json!({"user_name": "alice"}),
        "server.service.restart" => serde_json::json!({"name": "spot-runtime"}),
        "tenant.backup.run" => serde_json::json!({"passphrase": "hunter2"}),
        "system.disk.wipe" => serde_json::json!({"disk": "/dev/sda"}),
        other => panic!("no test parameters for {other}"),
    }
}

/// Every operation id in the test catalogue, in order.
pub fn operations() -> Vec<String> {
    catalogue()
        .iter()
        .map(|spec| spec.id.as_str().to_owned())
        .collect()
}

/// The fixture: catalogue, trust store, keys, log and a ready agent.
pub struct Harness {
    pub catalogue: Catalogue,
    pub trust: TrustStore,
    pub policy: Policy,
    pub agent_id: String,
    pub requester: SigningKey,
    pub admin_a: SigningKey,
    pub admin_b: SigningKey,
    pub admin_c: SigningKey,
    pub log: Arc<MemoryResultLog>,
}

impl Harness {
    /// A strict-policy harness whose four administrators are all hardware keys.
    pub fn new() -> Self {
        let mut trust = TrustStore::new();
        let requester = key(10);
        let admin_a = key(11);
        let admin_b = key(12);
        let admin_c = key(13);
        for signer in [&requester, &admin_a, &admin_b, &admin_c] {
            trust
                .insert(&signer.verifying_key().to_bytes(), Protection::Hardware)
                .unwrap();
        }
        Self {
            catalogue: catalogue(),
            trust,
            policy: Policy::default(),
            agent_id: "agent-1".to_owned(),
            requester,
            admin_a,
            admin_b,
            admin_c,
            log: Arc::new(MemoryResultLog::new("agent-1")),
        }
    }

    /// A fresh agent over this harness (its own clone of the store and catalogue).
    pub fn agent(&self) -> Agent {
        Agent::new(
            self.agent_id.clone(),
            self.catalogue.clone(),
            self.trust.clone(),
            self.policy.clone(),
            Arc::new(MemoryNonceStore::new()),
            Box::new(DelayOnly::new(0)),
            self.log.clone(),
        )
    }

    /// A command issued by the requester for this agent.
    pub fn command(
        &self,
        op_id: &str,
        params: serde_json::Value,
        issued_at: i64,
        expires_at: i64,
    ) -> Command {
        Command::new(
            op_id,
            params,
            self.agent_id.as_str(),
            key_id(&self.requester),
            issued_at,
            expires_at,
        )
    }

    /// A command for a chosen audience.
    pub fn command_for(
        &self,
        op_id: &str,
        audience: &str,
        issued_at: i64,
        expires_at: i64,
    ) -> Command {
        Command::new(
            op_id,
            valid_params(op_id),
            audience,
            key_id(&self.requester),
            issued_at,
            expires_at,
        )
    }

    /// Signs `command` with each signer, producing an envelope.
    pub fn envelope(&self, command: &Command, signers: &[&SigningKey]) -> CommandEnvelope {
        let signatures = signers
            .iter()
            .map(|signer| {
                let signature = signer.sign(&command.signed_message().unwrap());
                EnvelopeSignature::new(signer.verifying_key().to_bytes(), signature).unwrap()
            })
            .collect();
        CommandEnvelope::from_parts(
            String::from_utf8(command.canonical_bytes().unwrap()).unwrap(),
            signatures,
        )
        .unwrap()
    }

    /// The result log's record kinds, in order.
    pub fn log_kinds(&self) -> Vec<String> {
        self.log
            .entries()
            .unwrap()
            .into_iter()
            .map(|entry| entry.record.kind.clone())
            .collect()
    }
}
