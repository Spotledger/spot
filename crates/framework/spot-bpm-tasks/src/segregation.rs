//! The completion gate: creator-distinctness and segregation of duties.
//!
//! A Decision node may require that the person deciding is not the person who created the
//! underlying document (`distinct_from_creator`), and a task may declare the **duty** that
//! completing it exercises. [`DistinctFromCreator`] is the reference gate; [`SodSegregation`]
//! additionally asks the `spot-sod` analysis whether deciding would make the user hold both
//! sides of a rule, so an approval that breaches segregation is refused with the conflicting
//! rule named.

use spot_sod::{Assignment, GrantCheck, RoleCatalogue, RuleSet, SodOverride, check_grant};

use crate::task::Task;

/// Why the completion gate refused a decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegregationRefusal {
    /// The stable `bpm_tasks.*` code the refusal maps to.
    pub code: &'static str,
    /// A plain-language summary, naming the rule when there is one.
    pub detail: String,
}

/// Whether a user may decide a task.
pub trait Segregation: Send + Sync {
    /// Refuses the decision, or allows it.
    ///
    /// # Errors
    /// A [`SegregationRefusal`] naming the reason.
    fn may_decide(&self, task: &Task, user: &str) -> Result<(), SegregationRefusal>;
}

/// The reference gate: refuse when the node requires a decision distinct from the document's
/// creator and the deciding user is that creator.
#[derive(Debug, Clone, Copy, Default)]
pub struct DistinctFromCreator;

impl Segregation for DistinctFromCreator {
    fn may_decide(&self, task: &Task, user: &str) -> Result<(), SegregationRefusal> {
        if task.distinct_from_creator && task.created_by.as_deref() == Some(user) {
            return Err(SegregationRefusal {
                code: "bpm_tasks.distinct_from_creator",
                detail: format!("{user} created the underlying document"),
            });
        }
        Ok(())
    }
}

/// A gate that also consults the segregation-of-duties analysis.
///
/// The task's `duty`, when it has one, is treated as a proposed assignment to the deciding user;
/// when that assignment would create a conflict under the loaded rules, the decision is refused
/// with the conflicting rule named. Creator-distinctness is checked first, as for the reference
/// gate.
pub struct SodSegregation {
    rules: RuleSet,
    catalogue: RoleCatalogue,
    existing: Vec<Assignment>,
    overrides: Vec<SodOverride>,
}

impl SodSegregation {
    /// Builds the gate from the rules, the role catalogue, the user's existing assignments and
    /// any overrides.
    #[must_use]
    pub fn new(
        rules: RuleSet,
        catalogue: RoleCatalogue,
        existing: Vec<Assignment>,
        overrides: Vec<SodOverride>,
    ) -> Self {
        Self {
            rules,
            catalogue,
            existing,
            overrides,
        }
    }
}

impl Segregation for SodSegregation {
    fn may_decide(&self, task: &Task, user: &str) -> Result<(), SegregationRefusal> {
        if task.distinct_from_creator && task.created_by.as_deref() == Some(user) {
            return Err(SegregationRefusal {
                code: "bpm_tasks.distinct_from_creator",
                detail: format!("{user} created the underlying document"),
            });
        }
        let Some(duty) = &task.duty else {
            return Ok(());
        };
        let proposed = Assignment::duty(user, duty, task.scope.as_deref());
        match check_grant(
            &self.rules,
            &self.catalogue,
            &self.existing,
            &proposed,
            &self.overrides,
        ) {
            GrantCheck::Clear => Ok(()),
            GrantCheck::Conflicts(conflicts) => {
                let rules: Vec<String> = conflicts
                    .iter()
                    .map(|conflict| conflict.rule_id.clone())
                    .collect();
                Err(SegregationRefusal {
                    code: "bpm_tasks.sod_conflict",
                    detail: format!("the duty `{duty}` conflicts with {}", rules.join(", ")),
                })
            }
        }
    }
}
