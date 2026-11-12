//! Shared test helpers: invocation builders and a recording fake executor.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::sync::Mutex;

use async_trait::async_trait;
use spot_conditions::{HealthThresholds, Millis};
use spot_mgmt_ops::{Catalogue, Invocation, ParamValue, Validated};
use spot_sync_health::HealthView;

use spot_mgmt_sync::args::*;
use spot_mgmt_sync::results::*;
use spot_mgmt_sync::{ExecError, ExecKind, SyncOps, build_catalogue, ids};

pub fn catalogue() -> Catalogue {
    build_catalogue().unwrap()
}

pub fn invocation(op: &str, params: &[(&str, ParamValue)]) -> Invocation {
    let mut inv = Invocation::new(ids::op_id(op).unwrap());
    for (name, value) in params {
        inv = inv.with(*name, value.clone());
    }
    inv
}

pub fn text(value: &str) -> ParamValue {
    ParamValue::Text(value.to_owned())
}

pub fn list(values: &[&str]) -> ParamValue {
    ParamValue::List(values.iter().map(|v| text(v)).collect())
}

pub fn validated(op: &str, params: &[(&str, ParamValue)]) -> Validated {
    catalogue().validate(&invocation(op, params)).unwrap()
}

const NODE: &str = "branch07.spotledger.app";

/// One valid invocation per operation, in catalogue order, used by the round-trip and
/// catalogue tests.
pub fn samples() -> Vec<Invocation> {
    vec![
        invocation(ids::HEALTH, &[]),
        invocation(ids::NODE_LIST, &[]),
        invocation(ids::NODE_SHOW, &[("node", text(NODE))]),
        invocation(
            ids::NODE_ENROL,
            &[
                ("node", text(NODE)),
                ("role", text("edge")),
                ("partitions", list(&["sales", "stock"])),
                ("subscription", text("branch-standard")),
                ("valid_hours", ParamValue::Int(48)),
            ],
        ),
        invocation(ids::NODE_SUSPEND, &[("node", text(NODE))]),
        invocation(ids::NODE_RESUME, &[("node", text(NODE))]),
        invocation(
            ids::NODE_REVOKE,
            &[("node", text(NODE)), ("confirm", text(NODE))],
        ),
        invocation(ids::NODE_ROTATE_CERTIFICATE, &[("node", text(NODE))]),
        invocation(
            ids::NODE_RESEND,
            &[("node", text(NODE)), ("from", text("3:41"))],
        ),
        invocation(ids::NODE_NOTIFY_CONTACT, &[("node", text(NODE))]),
        invocation(ids::CONDITION_LIST, &[]),
        invocation(ids::QUARANTINE_LIST, &[]),
        invocation(ids::QUARANTINE_RETRY, &[("item", text("q-0192f1c0"))]),
        invocation(
            ids::QUARANTINE_DISCARD,
            &[
                ("item", text("q-0192f1c0")),
                ("reason_code", text("superseded")),
                ("confirm", text("q-0192f1c0")),
            ],
        ),
        invocation(ids::POLICY_SHOW, &[("doctype", text("Sales_Invoice"))]),
        invocation(ids::POLICY_LINT, &[]),
        invocation(
            ids::ROLE_CHANGE,
            &[("to", text("hub")), ("confirm", text("hub"))],
        ),
        invocation(ids::BOOTSTRAP_STATUS, &[]),
    ]
}

/// A fake executor that records which method ran, with what, and can be told to fail.
#[derive(Default)]
pub struct Fake {
    pub calls: Mutex<Vec<String>>,
    pub fail_with: Mutex<Option<ExecError>>,
}

impl Fake {
    pub fn failing(kind: ExecKind) -> Self {
        let fake = Self::default();
        *fake.fail_with.lock().unwrap() = Some(ExecError::new(kind, "refused_by_test"));
        fake
    }

    fn record(&self, call: String) -> Result<(), ExecError> {
        self.calls.lock().unwrap().push(call);
        match self.fail_with.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

pub const ENROL_CODE: &str = "k7q2-m9xw-4tfd";

fn pos() -> Position {
    Position {
        generation: 3,
        seq: 41,
    }
}

fn summary(node: &str, state: NodeLifecycle) -> NodeSummary {
    NodeSummary {
        node: node.to_owned(),
        role: NodeRole::Edge,
        state,
        last_contact: Some("2026-11-12T01:00:00Z".into()),
        version: Some("0.1.2".into()),
    }
}

#[async_trait]
impl SyncOps for Fake {
    async fn health(&self) -> Result<HealthView, ExecError> {
        self.record("health".into())?;
        Ok(spot_sync_health::build(
            &[],
            &[],
            Millis::new(1_000),
            &HealthThresholds::new(Millis::new(60_000)),
        ))
    }
    async fn node_list(&self) -> Result<NodeListResult, ExecError> {
        self.record("node_list".into())?;
        Ok(NodeListResult {
            nodes: vec![summary(NODE, NodeLifecycle::Active)],
        })
    }
    async fn node_show(&self, args: NodeArgs) -> Result<NodeShowResult, ExecError> {
        self.record(format!("node_show {}", args.node))?;
        Ok(NodeShowResult {
            node: summary(&args.node, NodeLifecycle::Active),
            partitions: vec!["sales".into()],
            subscription: "branch-standard".into(),
            certificate_expires_at: None,
            lag_positions: 0,
            acknowledged: Some(pos()),
        })
    }
    async fn node_enrol(&self, args: EnrolArgs) -> Result<EnrolResult, ExecError> {
        self.record(format!(
            "node_enrol {} {} {} {} {}",
            args.node,
            args.role.name(),
            args.partitions.join(","),
            args.subscription,
            args.valid_hours
        ))?;
        Ok(EnrolResult {
            enrolment_id: "enr-1".into(),
            node: args.node,
            expires_at: "2026-11-14T00:00:00Z".into(),
            code: Some(OneTimeCode::new(ENROL_CODE.to_owned())),
        })
    }
    async fn node_suspend(&self, args: NodeArgs) -> Result<NodeStateResult, ExecError> {
        self.record(format!("node_suspend {}", args.node))?;
        Ok(NodeStateResult {
            node: args.node,
            state: NodeLifecycle::Suspended,
        })
    }
    async fn node_resume(&self, args: NodeArgs) -> Result<NodeStateResult, ExecError> {
        self.record(format!("node_resume {}", args.node))?;
        Ok(NodeStateResult {
            node: args.node,
            state: NodeLifecycle::Active,
        })
    }
    async fn node_revoke(&self, args: NodeArgs) -> Result<NodeStateResult, ExecError> {
        self.record(format!("node_revoke {}", args.node))?;
        Ok(NodeStateResult {
            node: args.node,
            state: NodeLifecycle::Revoked,
        })
    }
    async fn node_rotate_certificate(
        &self,
        args: NodeArgs,
    ) -> Result<RotateCertificateResult, ExecError> {
        self.record(format!("node_rotate_certificate {}", args.node))?;
        Ok(RotateCertificateResult {
            node: args.node,
            certificate_expires_at: "2027-11-12T00:00:00Z".into(),
        })
    }
    async fn node_resend(&self, args: ResendArgs) -> Result<ResendResult, ExecError> {
        self.record(format!(
            "node_resend {} {}",
            args.node,
            args.from.map_or("none".to_owned(), |p| p.render())
        ))?;
        Ok(ResendResult {
            node: args.node,
            from: args.from.unwrap_or(pos()),
            queued_positions: 12,
        })
    }
    async fn node_notify_contact(&self, args: NodeArgs) -> Result<NotifyContactResult, ExecError> {
        self.record(format!("node_notify_contact {}", args.node))?;
        Ok(NotifyContactResult {
            node: args.node,
            notified: true,
        })
    }
    async fn condition_list(&self) -> Result<ConditionListResult, ExecError> {
        self.record("condition_list".into())?;
        Ok(ConditionListResult { conditions: vec![] })
    }
    async fn quarantine_list(&self) -> Result<QuarantineListResult, ExecError> {
        self.record("quarantine_list".into())?;
        Ok(QuarantineListResult { items: vec![] })
    }
    async fn quarantine_retry(
        &self,
        args: QuarantineRetryArgs,
    ) -> Result<QuarantineRetryResult, ExecError> {
        self.record(format!("quarantine_retry {}", args.item))?;
        Ok(QuarantineRetryResult {
            item: args.item,
            outcome: RetryOutcome::Applied,
        })
    }
    async fn quarantine_discard(
        &self,
        args: QuarantineDiscardArgs,
    ) -> Result<QuarantineDiscardResult, ExecError> {
        self.record(format!(
            "quarantine_discard {} {}",
            args.item, args.reason_code
        ))?;
        Ok(QuarantineDiscardResult {
            item: args.item,
            discarded: true,
        })
    }
    async fn policy_show(&self, args: PolicyShowArgs) -> Result<PolicyShowResult, ExecError> {
        self.record(format!("policy_show {}", args.doctype))?;
        Ok(PolicyShowResult {
            doctype: args.doctype,
            preset: "record".into(),
            adopt: None,
        })
    }
    async fn policy_lint(&self) -> Result<PolicyLintResult, ExecError> {
        self.record("policy_lint".into())?;
        Ok(PolicyLintResult {
            clean: true,
            findings: vec![],
        })
    }
    async fn role_change(&self, args: RoleChangeArgs) -> Result<RoleChangeResult, ExecError> {
        self.record(format!("role_change {}", args.to.name()))?;
        Ok(RoleChangeResult {
            from: NodeRole::Edge,
            to: args.to,
        })
    }
    async fn bootstrap_status(&self) -> Result<BootstrapStatusResult, ExecError> {
        self.record("bootstrap_status".into())?;
        Ok(BootstrapStatusResult {
            state: BootstrapState::Running,
            ranges_total: 10,
            ranges_done: 4,
            position: Some(pos()),
        })
    }
}

/// The given bad values plus one of `long` characters, as owned strings.
pub fn bad_set(values: &[&str], long: usize) -> Vec<String> {
    let mut all: Vec<String> = values.iter().map(|v| (*v).to_owned()).collect();
    all.push("a".repeat(long));
    all
}
