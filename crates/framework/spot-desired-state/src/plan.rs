//! Bulk remediation plans: typed catalogue operations, hashed and approved before apply.
//!
//! A [`BulkPlan`] is a list of [`PlanStep`]s, one per server, and each step is a list of
//! [`TypedOperation`]s from the management catalogue. A single-server remediation is a plan of
//! size one. The plan's [`BulkPlan::digest`] binds approval and rehearsal to exactly what apply
//! will run.
//!
//! [`remediate`] turns drifts into a plan. It never executes anything: an approved plan plus a
//! passed rehearsal are the two proofs the run requires.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use spot_fleet_rollout::NodeId;
use spot_mgmt_ops::{OpId, ParamValue, Risk};

use crate::drift::Drift;
use crate::ids::PlanDigest;

/// One typed catalogue operation, with its parameters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct TypedOperation {
    /// The catalogue operation id.
    pub id: OpId,
    /// The operation's parameters, by name.
    pub params: BTreeMap<String, ParamValue>,
}

impl TypedOperation {
    /// An operation.
    #[must_use]
    pub fn new(id: OpId, params: BTreeMap<String, ParamValue>) -> Self {
        Self { id, params }
    }

    /// A stable text form, used to deduplicate identical operations.
    #[must_use]
    pub fn canonical(&self) -> String {
        let params = serde_json::to_string(&self.params).unwrap_or_default();
        format!("{}?{}", self.id.as_str(), params)
    }
}

/// One step of a bulk plan: everything to do on one server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct PlanStep {
    /// A stable id within the plan, for reports (`<server>/<ordinal>`).
    pub id: String,
    /// The server the step runs on.
    pub server: NodeId,
    /// The server's group.
    pub group: String,
    /// The typed operations, in application order.
    pub operations: Vec<TypedOperation>,
    /// The highest risk among the drifts this step remediates.
    pub risk: Risk,
}

/// A drift the policy kept out of the plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct SkippedDrift {
    /// The server.
    pub server: NodeId,
    /// What drifted.
    pub subject: crate::drift::DriftSubject,
    /// The drift's risk.
    pub risk: Risk,
    /// Why the policy kept it out.
    pub reason: String,
}

/// A bulk change, ready to be approved, rehearsed, applied and verified.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct BulkPlan {
    /// The group the plan covers; a plan normally covers exactly one group.
    pub group: String,
    /// One step per server, in server order.
    pub steps: Vec<PlanStep>,
    /// Drifts the policy excluded, for the console to show beside the plan.
    pub skipped: Vec<SkippedDrift>,
    /// The highest risk in the plan.
    pub risk: Risk,
}

impl BulkPlan {
    /// Whether the plan has no step.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The number of server steps.
    #[must_use]
    pub fn size(&self) -> usize {
        self.steps.len()
    }

    /// The number of typed operations across every step.
    #[must_use]
    pub fn operation_count(&self) -> usize {
        self.steps.iter().map(|step| step.operations.len()).sum()
    }

    /// Every server the plan touches, in id order.
    #[must_use]
    pub fn servers(&self) -> BTreeSet<NodeId> {
        self.steps.iter().map(|step| step.server.clone()).collect()
    }

    /// Every group the plan touches, in name order.
    #[must_use]
    pub fn groups(&self) -> BTreeSet<String> {
        self.steps.iter().map(|step| step.group.clone()).collect()
    }

    /// The plan's domain-separated digest over exactly what apply will run.
    ///
    /// The digest covers the group, every step's id, server and operations, in order, and the
    /// plan's risk. It does not cover the skipped drifts (they are not applied). A caller that
    /// changes any step gets a different digest, so an old approval never matches.
    #[must_use]
    pub fn digest(&self) -> PlanDigest {
        #[derive(Serialize)]
        struct Canonical<'a> {
            group: &'a str,
            risk: Risk,
            steps: &'a [PlanStep],
        }
        let canonical = Canonical {
            group: &self.group,
            risk: self.risk,
            steps: &self.steps,
        };
        let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
        PlanDigest::of_canonical(&bytes)
    }

    /// Rehearses the plan structurally. See [`crate::rehearse`].
    #[must_use]
    pub fn rehearse(&self) -> crate::RehearsalReport {
        crate::rehearse(self)
    }
}

/// How [`remediate`] decides what goes into the automatic plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemediationPolicy {
    /// Drifts at or below this risk are remediated automatically; higher ones are skipped for
    /// a person to handle.
    pub automatic_up_to: Risk,
    /// Whether package drift is included.
    pub include_packages: bool,
    /// Whether service drift is included.
    pub include_services: bool,
}

impl RemediationPolicy {
    /// Remediate everything up to medium risk, packages and services included.
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            automatic_up_to: Risk::Medium,
            include_packages: true,
            include_services: true,
        }
    }
}

impl Default for RemediationPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

/// Turns drifts into a bulk plan: one step per server, one operation list per step.
///
/// The input need not be sorted; the result is deterministic (drifts sorted by server then
/// subject, steps in server order, identical operations deduplicated). One drift from one
/// server therefore yields a plan of size one.
#[must_use]
pub fn remediate(drifts: &[Drift], policy: &RemediationPolicy) -> BulkPlan {
    let mut ordered: Vec<&Drift> = drifts.iter().collect();
    ordered.sort_by(|left, right| {
        left.server
            .cmp(&right.server)
            .then_with(|| left.subject.cmp(&right.subject))
    });

    let mut skipped = Vec::new();
    let mut by_server: BTreeMap<NodeId, Vec<&Drift>> = BTreeMap::new();
    let group = ordered
        .first()
        .map_or_else(String::new, |drift| drift.group.clone());
    for drift in ordered {
        let excluded = if drift.risk > policy.automatic_up_to {
            Some(format!(
                "risk {} is above the automatic limit {}",
                risk_name(drift.risk),
                risk_name(policy.automatic_up_to)
            ))
        } else if drift.subject.is_package() && !policy.include_packages {
            Some("package drift is outside the policy".to_owned())
        } else if drift.subject.is_service() && !policy.include_services {
            Some("service drift is outside the policy".to_owned())
        } else if drift.remediation.is_empty() {
            Some("the drift has no catalogue remediation".to_owned())
        } else {
            None
        };
        if let Some(reason) = excluded {
            skipped.push(SkippedDrift {
                server: drift.server.clone(),
                subject: drift.subject.clone(),
                risk: drift.risk,
                reason,
            });
            continue;
        }
        by_server
            .entry(drift.server.clone())
            .or_default()
            .push(drift);
    }

    let mut steps = Vec::with_capacity(by_server.len());
    for (server, server_drifts) in by_server {
        let mut seen = BTreeSet::new();
        let mut operations = Vec::new();
        let mut risk = Risk::Read;
        for drift in &server_drifts {
            risk = risk.max(drift.risk);
            for operation in &drift.remediation {
                if seen.insert(operation.canonical()) {
                    operations.push(operation.clone());
                }
            }
        }
        let ordinal = steps.len().saturating_add(1);
        steps.push(PlanStep {
            id: format!("{server}/{ordinal}"),
            server,
            group: server_drifts
                .first()
                .map_or_else(String::new, |drift| drift.group.clone()),
            operations,
            risk,
        });
    }

    let risk = steps
        .iter()
        .map(|step| step.risk)
        .max()
        .unwrap_or(Risk::Read);

    BulkPlan {
        group,
        steps,
        skipped,
        risk,
    }
}

/// The lower-case catalogued name of a risk.
#[must_use]
pub fn risk_name(risk: Risk) -> &'static str {
    match risk {
        Risk::Read => "read",
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
        Risk::Critical => "critical",
    }
}

/// Builds an operation from an id and named parameters.
#[must_use]
pub fn operation(
    id: &OpId,
    params: impl IntoIterator<Item = (&'static str, ParamValue)>,
) -> TypedOperation {
    TypedOperation::new(
        id.clone(),
        params
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}
