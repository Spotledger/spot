//! Shared test doubles and builders. Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeMap;

use spot_conditions::Millis;
use spot_storage_contract::{Name, Version};
use spot_sync_healing::{
    ActionKind, ActionOutcome, ActionRequest, Observation, RepairActions, Target,
};
use spot_sync_reconcile::{Finding, Measure, NodeRole, ReconcileReport, Scope};

/// Records every call; answers per kind, with an override per target.
#[derive(Default)]
pub struct Script {
    pub calls: Vec<(ActionKind, String, Target)>,
    pub by_kind: BTreeMap<ActionKind, ActionOutcome>,
    pub fail_all: Option<String>,
}

impl Script {
    pub fn failing(reason: &str) -> Self {
        Self {
            fail_all: Some(reason.to_owned()),
            ..Self::default()
        }
    }

    pub fn answering(kind: ActionKind, outcome: ActionOutcome) -> Self {
        let mut script = Self::default();
        script.by_kind.insert(kind, outcome);
        script
    }

    fn answer(&mut self, kind: ActionKind, request: &ActionRequest<'_>) -> ActionOutcome {
        self.calls
            .push((kind, request.node.to_owned(), request.target.clone()));
        if let Some(reason) = &self.fail_all {
            return ActionOutcome::Failed(reason.clone());
        }
        self.by_kind
            .get(&kind)
            .cloned()
            .unwrap_or(ActionOutcome::Done)
    }
}

impl RepairActions for Script {
    fn rebuild_digest(&mut self, r: &ActionRequest<'_>) -> ActionOutcome {
        self.answer(ActionKind::RebuildDigest, r)
    }
    fn repull_record(&mut self, r: &ActionRequest<'_>) -> ActionOutcome {
        self.answer(ActionKind::RepullRecord, r)
    }
    fn resend_scope(&mut self, r: &ActionRequest<'_>) -> ActionOutcome {
        self.answer(ActionKind::ResendScope, r)
    }
    fn request_snapshot(&mut self, r: &ActionRequest<'_>) -> ActionOutcome {
        self.answer(ActionKind::RequestSnapshot, r)
    }
}

pub fn ms(n: u64) -> Millis {
    Millis::new(n)
}

pub fn scope() -> Scope {
    Scope::new("company", "C1")
}

pub fn missing(name: &str) -> Finding {
    Finding::MissingAtReceiver {
        entity: Name::new("item").unwrap(),
        name: name.into(),
        version: Version(3),
    }
}

pub fn behind(name: &str) -> Finding {
    Finding::VersionMismatch {
        entity: Name::new("item").unwrap(),
        name: name.into(),
        local: Version(5),
        remote: Version(2),
    }
}

pub fn ahead(name: &str) -> Finding {
    Finding::VersionMismatch {
        entity: Name::new("item").unwrap(),
        name: name.into(),
        local: Version(2),
        remote: Version(5),
    }
}

pub fn orphan(name: &str) -> Finding {
    Finding::ExtraAtReceiver {
        entity: Name::new("item").unwrap(),
        name: name.into(),
        version: Version(1),
    }
}

pub fn total() -> Finding {
    Finding::TotalMismatch {
        measure: Measure::GlBalance {
            account: "1100".into(),
        },
        local: 100,
        remote: 90,
    }
}

pub fn report(scope: Scope, findings: Vec<Finding>) -> ReconcileReport {
    ReconcileReport {
        scope,
        findings,
        rounds: 1,
        digest_bytes: 64,
    }
}

/// An owner-to-mirror observation of the default scope.
pub fn obs(node: &str, findings: Vec<Finding>) -> Observation {
    obs_in(node, scope(), findings)
}

pub fn obs_in(node: &str, scope: Scope, findings: Vec<Finding>) -> Observation {
    Observation {
        node: node.into(),
        report: report(scope, findings),
        source_role: Some(NodeRole::Owner),
        destination_role: Some(NodeRole::Mirror),
    }
}
