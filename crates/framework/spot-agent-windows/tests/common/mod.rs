//! Shared helpers for the Windows module integration tests.
//!
//! The tests build real [`ExecutionPlan`]s from the catalogue's own validation, so the
//! arguments a handler sees are exactly what a signed command would carry, and drive them
//! through the [`HandlerRegistry`] the way the executor does. Everything runs on Linux
//! against [`FakeWin`].

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use spot_agent_core::{ExecutionPlan, PlanParts};
use spot_agent_core::{HandlerOutcome, HandlerRegistry};
use spot_agent_windows::secrets::{PasswordSource, SealedResultChannel, Secret, SecretLabel};
use spot_agent_windows::{AgentConfig, FakeWin, WindowsError, handlers};
use spot_mgmt_ops::{Catalogue, Invocation, OpId, ParamValue, Risk};
use spot_signed_command::KeyId;

/// A sealed result channel that records only the account and the secret's length, never
/// the value, so a test can prove delivery without holding a secret.
#[derive(Default)]
pub struct RecordingChannel {
    delivered: Mutex<Vec<(String, usize)>>,
}

impl RecordingChannel {
    /// The delivered `(account, length)` pairs.
    pub fn delivered(&self) -> Vec<(String, usize)> {
        self.delivered.lock().unwrap().clone()
    }
}

impl SealedResultChannel for RecordingChannel {
    fn deliver(&self, label: &SecretLabel, secret: Secret) -> Result<(), WindowsError> {
        self.delivered
            .lock()
            .unwrap()
            .push((label.account.clone(), secret.len()));
        Ok(())
    }
}

/// A deterministic password source for the tests.
pub fn passwords() -> Arc<dyn PasswordSource> {
    Arc::new(spot_agent_windows::secrets::FixedPasswordSource::new(
        "Fixed-Passw0rd!Fixed-Pass",
    ))
}

/// The catalogue of the Windows module.
pub fn catalogue() -> Catalogue {
    spot_agent_windows::catalogue::catalogue().unwrap()
}

/// A registry with every Windows handler, over `api`, plus the recording channel.
pub fn registry(api: Arc<FakeWin>, channel: Arc<RecordingChannel>) -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    handlers::register(
        &mut registry,
        api,
        AgentConfig::default(),
        passwords(),
        channel,
    );
    registry
}

/// A plan for `op` with the given parameters, validated by the catalogue.
pub fn plan(catalogue: &Catalogue, op: &str, params: Vec<(&str, ParamValue)>) -> ExecutionPlan {
    let mut invocation = Invocation::new(OpId::new(op).unwrap());
    for (name, value) in params {
        invocation.params.insert(name.to_owned(), value);
    }
    let validated = catalogue.validate(&invocation).unwrap();
    ExecutionPlan::new(PlanParts {
        agent_id: "agent-1".to_owned(),
        command_id: "cmd-1".to_owned(),
        command_digest: [0u8; 32],
        op: validated.op.clone(),
        args: validated,
        risk: Risk::Medium,
        signers: vec![KeyId::from_public_key(&[1u8; 32])],
        issued_at: 1_000,
    })
}

/// Runs `op` through its registered handler and returns the outcome.
pub fn run(
    registry: &HandlerRegistry,
    catalogue: &Catalogue,
    op: &str,
    params: Vec<(&str, ParamValue)>,
) -> HandlerOutcome {
    let plan = plan(catalogue, op, params);
    let handler = registry
        .get(plan.op())
        .expect("every catalogue operation has a handler");
    handler.handle(&plan)
}

/// Whether an outcome succeeded.
pub fn succeeded(outcome: &HandlerOutcome) -> bool {
    matches!(outcome, HandlerOutcome::Succeeded)
}

/// Text parameter helper.
pub fn text(value: &str) -> ParamValue {
    ParamValue::Text(value.to_owned())
}

/// Boolean parameter helper.
pub fn boolean(value: bool) -> ParamValue {
    ParamValue::Bool(value)
}

/// Integer parameter helper.
pub fn integer(value: i64) -> ParamValue {
    ParamValue::Int(value)
}

/// Duration parameter helper.
pub fn duration(value: u64) -> ParamValue {
    ParamValue::Duration(value)
}

/// A list of text parameter helper.
pub fn text_list(values: &[&str]) -> ParamValue {
    ParamValue::List(values.iter().map(|value| text(value)).collect())
}

/// A list of integers parameter helper.
pub fn int_list(values: &[i64]) -> ParamValue {
    ParamValue::List(values.iter().map(|value| integer(*value)).collect())
}
