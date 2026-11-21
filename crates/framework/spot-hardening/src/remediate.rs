//! Turning a failing report into remediation: the operations to apply and the operations that
//! are only suggested.
//!
//! Only a control that failed, carries a remedy and is reversible is a candidate. A candidate
//! is *applied* only when the policy allows it: the mode must be
//! [`PolicyMode::AutoRemediate`](crate::model::PolicyMode::AutoRemediate), the control must be
//! at or above the policy's minimum severity, and a lock-out-risk candidate (an SSH or
//! firewall change) needs both the policy's permission and a connectivity guard. Every other
//! candidate is *suggested*: listed for a human to approve, never applied on its own.
//!
//! The plan is deterministic: it follows the report's order, and identical operations (the two
//! SSH controls share one remedy) appear once.

use crate::model::{OpRef, Policy, PolicyMode};
use crate::report::{ControlResult, Report};

/// The operations the policy permits the agent to apply now, in report order and without
/// duplicates.
#[must_use]
pub fn remediation_plan(report: &Report, policy: &Policy) -> Vec<OpRef> {
    if policy.mode != PolicyMode::AutoRemediate {
        return Vec::new();
    }
    let mut plan: Vec<OpRef> = Vec::new();
    for candidate in candidates(report, policy) {
        if candidate.lockout_risk && !policy.may_apply_lockout() {
            continue;
        }
        if let Some(remedy) = &candidate.remedy
            && !plan.contains(remedy)
        {
            plan.push(remedy.clone());
        }
    }
    plan
}

/// The operations that address a failing control but must be approved first: the lock-out-risk
/// candidates when the policy withholds them, and every candidate when the mode is
/// suggest-only. In report order and without duplicates.
#[must_use]
pub fn suggested_plan(report: &Report, policy: &Policy) -> Vec<OpRef> {
    let suggest_all = policy.mode != PolicyMode::AutoRemediate;
    let mut plan: Vec<OpRef> = Vec::new();
    for candidate in candidates(report, policy) {
        let held_back = suggest_all || (candidate.lockout_risk && !policy.may_apply_lockout());
        if held_back
            && let Some(remedy) = &candidate.remedy
            && !plan.contains(remedy)
        {
            plan.push(remedy.clone());
        }
    }
    plan
}

/// The failing, remediable, reversible controls at or above the policy's minimum severity, in
/// report order.
fn candidates<'a>(report: &'a Report, policy: &Policy) -> impl Iterator<Item = &'a ControlResult> {
    report.results.iter().filter(move |result| {
        result.outcome.is_fail()
            && result.remedy.is_some()
            && result.reversible
            && result.severity >= policy.minimum_severity
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::model::{ControlId, OsFamily, Severity};
    use crate::report::{ControlResult, Outcome, Score};
    use spot_mgmt_ops::{OpId, ParamValue};

    fn op(text: &str) -> OpRef {
        OpRef::new(OpId::new(text).unwrap())
    }

    fn result(
        id: &str,
        outcome: Outcome,
        remedy: Option<OpRef>,
        reversible: bool,
        lockout_risk: bool,
        severity: Severity,
    ) -> ControlResult {
        ControlResult {
            id: ControlId::try_new(id).unwrap(),
            title: "a control".to_owned(),
            severity,
            outcome,
            remedy,
            reversible,
            lockout_risk,
        }
    }

    fn report(results: Vec<ControlResult>) -> Report {
        let score = Score::of(&results);
        Report {
            os_family: OsFamily::Linux,
            results,
            score,
        }
    }

    fn fail() -> Outcome {
        Outcome::Fail {
            found: "bad".to_owned(),
            expected: "good".to_owned(),
        }
    }

    fn ssh_report() -> Report {
        report(vec![
            result(
                "ssh.root_login",
                fail(),
                Some(op("os.service.disable").with("unit", ParamValue::Text("sshd".to_owned()))),
                true,
                true,
                Severity::High,
            ),
            result(
                "ssh.password_auth",
                fail(),
                Some(op("os.service.disable").with("unit", ParamValue::Text("sshd".to_owned()))),
                true,
                true,
                Severity::High,
            ),
            result(
                "time.sync",
                fail(),
                Some(op("os.service.enable").with("unit", ParamValue::Text("chronyd".to_owned()))),
                true,
                false,
                Severity::Medium,
            ),
            result("mac.enforcing", fail(), None, false, false, Severity::High),
        ])
    }

    #[test]
    fn suggest_only_mode_applies_nothing() {
        let report = ssh_report();
        let policy = Policy::suggest();
        assert!(remediation_plan(&report, &policy).is_empty());
        assert_eq!(suggested_plan(&report, &policy).len(), 2);
    }

    #[test]
    fn auto_mode_withholds_lockout_risk_without_a_guard() {
        let report = ssh_report();
        let policy = Policy::auto(Severity::Low);
        let plan = remediation_plan(&report, &policy);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].op.as_str(), "os.service.enable");
        let suggested = suggested_plan(&report, &policy);
        assert_eq!(suggested.len(), 1);
        assert_eq!(suggested[0].op.as_str(), "os.service.disable");
    }

    #[test]
    fn a_guard_lets_lockout_risk_apply_and_deduplicates_shared_remedies() {
        let report = ssh_report();
        let policy = Policy::auto(Severity::Low).with_lockout_guard(true);
        let plan = remediation_plan(&report, &policy);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].op.as_str(), "os.service.disable");
        assert_eq!(plan[1].op.as_str(), "os.service.enable");
        assert!(suggested_plan(&report, &policy).is_empty());
    }

    #[test]
    fn minimum_severity_filters_candidates() {
        let report = ssh_report();
        let policy = Policy::auto(Severity::High).with_lockout_guard(true);
        let plan = remediation_plan(&report, &policy);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].op.as_str(), "os.service.disable");
    }

    #[test]
    fn passing_and_non_reversible_controls_are_never_planned() {
        let report = report(vec![
            result(
                "ssh.root_login",
                Outcome::Pass,
                Some(op("os.service.disable")),
                true,
                true,
                Severity::High,
            ),
            result(
                "mac.enforcing",
                fail(),
                Some(op("os.setenforce")),
                false,
                false,
                Severity::High,
            ),
        ]);
        let policy = Policy::auto(Severity::Low).with_lockout_guard(true);
        assert!(remediation_plan(&report, &policy).is_empty());
    }
}
