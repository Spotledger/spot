//! Privilege separation: the broker's signed plan, and the executor's independent
//! re-derivation that refuses every forged plan.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use futures::executor::block_on;
use std::sync::Arc;

use common::Harness;
use spot_agent_core::{
    Broker, DelayOnly, ExecutionPlan, ExecutionStatus, Executor, ExecutorBuilder, HandlerOutcome,
    HandlerRegistry, InProcessChannel, LocalChannel, OperationHandler, PlanEnvelope, PlanParts,
    Refusal,
};
use spot_mgmt_ops::OpId;
use spot_signed_command::{KeyId, SignedCommand, Signer as _, SoftwareSigner};

struct Succeed;

impl OperationHandler for Succeed {
    fn handle(&self, _plan: &ExecutionPlan) -> HandlerOutcome {
        HandlerOutcome::Succeeded
    }
}

/// A registry with handlers for the two operations these tests drive.
fn registry() -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    registry.register(
        OpId::new("server.service.restart").unwrap(),
        Arc::new(Succeed),
    );
    registry.register(OpId::new("tenant.backup.run").unwrap(), Arc::new(Succeed));
    registry
}

struct Rig {
    broker: Broker,
    executor: Executor,
    broker_signer: Arc<SoftwareSigner>,
}

impl Rig {
    fn new(harness: &Harness, handlers: HandlerRegistry) -> Self {
        let broker_signer = Arc::new(SoftwareSigner::from_seed(&[20u8; 32]).unwrap());
        let broker = Broker::new(harness.agent(), broker_signer.clone());
        let executor = ExecutorBuilder::new(
            "agent-1",
            harness.catalogue.clone(),
            harness.trust.clone(),
            harness.policy.clone(),
            Box::new(DelayOnly::new(0)),
            harness.log.clone(),
        )
        .broker(broker.key_id(), broker_signer.public_key_bytes())
        .handlers(handlers)
        .build()
        .unwrap();
        Self {
            broker,
            executor,
            broker_signer,
        }
    }
}

#[test]
fn a_valid_plan_round_trips_over_the_channel_and_runs() {
    let harness = Harness::new();
    let rig = Rig::new(&harness, registry());
    let channel = InProcessChannel::shared();
    let command = harness.command(
        "server.service.restart",
        common::valid_params("server.service.restart"),
        970,
        1_600,
    );
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    let sent = block_on(rig.broker.route(&envelope, 1_000, &*channel)).unwrap();
    let received = channel.recv().unwrap().unwrap();
    assert_eq!(received.plan.plan_hash(), sent.plan.plan_hash());
    assert!(channel.is_empty().unwrap());
    let outcome = block_on(rig.executor.dispatch(&received, 1_000)).unwrap();
    assert_eq!(outcome.status, ExecutionStatus::Succeeded);
    assert_eq!(outcome.op, OpId::new("server.service.restart").unwrap());
    // The shared log recorded the broker's decision and the executor's result.
    assert!(harness.log.verify());
    assert_eq!(
        harness.log_kinds(),
        vec!["agent_core.execute", "agent_core.executed"]
    );
}

#[test]
fn a_forged_plan_with_a_downgraded_risk_is_refused() {
    let harness = Harness::new();
    let rig = Rig::new(&harness, registry());
    let command = harness.command(
        "tenant.backup.run",
        common::valid_params("tenant.backup.run"),
        970,
        1_600,
    );
    let envelope = harness.envelope(&command, &[&harness.admin_a, &harness.admin_b]);
    let plan = block_on(rig.broker.plan(&envelope, 1_000)).unwrap().plan;
    // The compromised broker re-labels a high operation as low and signs the false plan.
    let forged = ExecutionPlan::new(PlanParts {
        agent_id: plan.agent_id().to_owned(),
        command_id: plan.command_id().to_owned(),
        command_digest: *plan.command_digest(),
        op: plan.op().clone(),
        args: plan.args().clone(),
        risk: spot_mgmt_ops::Risk::Low,
        signers: plan.signers().to_vec(),
        issued_at: plan.issued_at(),
    });
    let broker_signature = rig.broker_signer.sign(forged.plan_hash());
    let forged_envelope = PlanEnvelope {
        command: envelope.to_signed().unwrap(),
        plan: forged,
        broker_key_id: rig.broker.key_id(),
        broker_signature,
    };
    assert_eq!(
        block_on(rig.executor.dispatch(&forged_envelope, 1_000)),
        Err(Refusal::PlanUnverified)
    );
}

#[test]
fn an_unsigned_command_cannot_be_laundered_through_the_broker() {
    let harness = Harness::new();
    let rig = Rig::new(&harness, registry());
    let command = harness.command(
        "tenant.backup.run",
        common::valid_params("tenant.backup.run"),
        970,
        1_600,
    );
    let signed = harness.envelope(&command, &[&harness.admin_a, &harness.admin_b]);
    let sent = block_on(rig.broker.plan(&signed, 1_000)).unwrap();
    // Strip the administrator signatures; the plan is unchanged, but the executor
    // re-derives from a command that no longer carries proof.
    let unsigned = PlanEnvelope {
        command: SignedCommand {
            command: sent.command.command.clone(),
            signatures: Vec::new(),
        },
        plan: sent.plan,
        broker_key_id: sent.broker_key_id,
        broker_signature: sent.broker_signature,
    };
    assert_eq!(
        block_on(rig.executor.dispatch(&unsigned, 1_000)),
        Err(Refusal::InsufficientSignatures { have: 0, need: 2 })
    );
}

#[test]
fn an_untrusted_broker_key_is_refused() {
    let harness = Harness::new();
    let rig = Rig::new(&harness, registry());
    let command = harness.command(
        "server.service.restart",
        common::valid_params("server.service.restart"),
        970,
        1_600,
    );
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    let plan = block_on(rig.broker.plan(&envelope, 1_000)).unwrap();
    let unknown = PlanEnvelope {
        broker_key_id: KeyId::from_public_key(&[21u8; 32]),
        ..plan
    };
    assert_eq!(
        block_on(rig.executor.dispatch(&unknown, 1_000)),
        Err(Refusal::UntrustedBroker)
    );
}

#[test]
fn a_bad_broker_signature_is_refused() {
    let harness = Harness::new();
    let rig = Rig::new(&harness, registry());
    let command = harness.command(
        "server.service.restart",
        common::valid_params("server.service.restart"),
        970,
        1_600,
    );
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    let mut plan = block_on(rig.broker.plan(&envelope, 1_000)).unwrap();
    plan.broker_signature = rig.broker_signer.sign(b"not the plan");
    assert_eq!(
        block_on(rig.executor.dispatch(&plan, 1_000)),
        Err(Refusal::BrokerSignatureInvalid)
    );
}

#[test]
fn an_operation_without_a_handler_is_refused() {
    let harness = Harness::new();
    // A registry without the medium operation.
    let rig = Rig::new(&harness, HandlerRegistry::new());
    let command = harness.command(
        "server.service.restart",
        common::valid_params("server.service.restart"),
        970,
        1_600,
    );
    let envelope = harness.envelope(&command, &[&harness.admin_a]);
    let plan = block_on(rig.broker.plan(&envelope, 1_000)).unwrap();
    assert_eq!(
        block_on(rig.executor.dispatch(&plan, 1_000)),
        Err(Refusal::UnknownHandler {
            op: "server.service.restart".to_owned()
        })
    );
}
