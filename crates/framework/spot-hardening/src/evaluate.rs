//! Evaluation: a baseline plus a fact source become a [`Report`].
//!
//! One result per control, in the baseline's declaration order, so the report is stable and a
//! diff between two reports is meaningful. Evaluation never fails as a whole: a control whose
//! fact source errors is reported as an error and the rest still run.

use crate::baseline::Baseline;
use crate::check::check_outcome;
use crate::facts::FactSource;
use crate::report::{ControlResult, Report, Score};

/// Evaluates every control in `baseline` against `source`.
#[must_use]
pub fn evaluate(baseline: &Baseline, source: &dyn FactSource) -> Report {
    let results: Vec<ControlResult> = baseline
        .controls
        .iter()
        .map(|control| ControlResult {
            id: control.id.clone(),
            title: control.title.clone(),
            severity: control.severity,
            outcome: check_outcome(&control.check, source),
            remedy: control.remedy.clone(),
            reversible: control.reversible,
            lockout_risk: control.lockout_risk,
        })
        .collect();
    let score = Score::of(&results);
    Report {
        os_family: baseline.os_family,
        results,
        score,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::error::HardeningError;
    use crate::facts::Facts;
    use crate::model::{ControlId, FactKey, FactValue};

    #[test]
    fn a_report_has_one_result_per_control_in_order() {
        let baseline = Baseline::linux().unwrap();
        let facts = Facts::new();
        let report = evaluate(&baseline, &facts);
        assert_eq!(report.results.len(), baseline.controls.len());
        for (result, control) in report.results.iter().zip(&baseline.controls) {
            assert_eq!(result.id, control.id);
        }
    }

    #[test]
    fn a_failing_fact_source_errors_only_that_control() {
        struct Broken;
        impl FactSource for Broken {
            fn fact(&self, key: &FactKey) -> Result<Option<FactValue>, HardeningError> {
                Err(HardeningError::FactUnavailable {
                    fact: key.as_str().to_owned(),
                    detail: "the agent is not enrolled".to_owned(),
                })
            }
        }
        let baseline = Baseline::linux().unwrap();
        let report = evaluate(&baseline, &Broken);
        assert_eq!(report.score.errored, baseline.controls.len());
        assert_eq!(report.score.passed, 0);
        let id = ControlId::try_new("time.sync").unwrap();
        assert!(!report.result(&id).unwrap().outcome.is_fail());
    }

    #[test]
    fn a_compliant_linux_host_passes_the_remediable_controls() {
        let baseline = Baseline::linux().unwrap();
        let facts = Facts::new()
            .with(
                FactKey::try_new("ssh.enabled").unwrap(),
                FactValue::Bool(false),
            )
            .with(
                FactKey::try_new("time.synchronised").unwrap(),
                FactValue::Bool(true),
            );
        let report = evaluate(&baseline, &facts);
        for result in &report.results {
            if result.auto_remediable() {
                assert!(
                    !result.outcome.is_fail(),
                    "{} should not fail",
                    result.id.as_str()
                );
            }
        }
    }
}
