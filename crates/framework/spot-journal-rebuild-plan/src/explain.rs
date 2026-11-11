//! The plan as typed lines for the recovery report: a stable key and typed params, never
//! translated text.
//!
//! The keys are hyphenated rather than dotted on purpose: a dotted lower-case string literal
//! outside a comment is what `tools/lints/error-codes/` reads as an error code, and these are
//! not error codes.

use crate::base::Base;
use crate::plan::RebuildPlan;

/// One line of the explanation: a stable key and the typed params the message braces.
///
/// The renderer looks the key up in the message catalogue and substitutes the params, so this
/// crate carries no English sentence of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplainLine {
    /// The stable key, hyphenated: `journal-rebuild-base-snapshot` and friends.
    pub key: &'static str,
    /// The params the message braces, in the order the sentence uses them.
    pub params: Vec<(&'static str, String)>,
}

impl ExplainLine {
    /// A line with its params, in message order.
    fn new(key: &'static str, params: Vec<(&'static str, String)>) -> Self {
        Self { key, params }
    }

    /// The value of one param, when the line carries it.
    #[must_use]
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(param, _)| *param == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Explains a plan in the order it is applied: the base, the steps, where it ends, what happens
/// to the generation and the positions, then the two phases that always follow.
#[must_use]
pub fn explain(plan: &RebuildPlan) -> Vec<ExplainLine> {
    let mut lines: Vec<ExplainLine> = Vec::new();
    match plan.base {
        Base::Snapshot {
            position,
            generation,
        } => lines.push(ExplainLine::new(
            "journal-rebuild-base-snapshot",
            vec![
                ("generation", generation.to_string()),
                ("position", position.to_string()),
            ],
        )),
        Base::Backup { position } => lines.push(ExplainLine::new(
            "journal-rebuild-base-backup",
            vec![("position", position.to_string())],
        )),
        Base::Empty => lines.push(ExplainLine::new("journal-rebuild-base-empty", Vec::new())),
    }
    for step in &plan.steps {
        lines.push(ExplainLine::new(
            "journal-rebuild-apply-segment",
            vec![
                ("first_seq", step.first_seq().to_string()),
                ("generation", step.generation().to_string()),
                ("last_seq", step.last_seq().to_string()),
            ],
        ));
    }
    lines.push(ExplainLine::new(
        "journal-rebuild-ends-at",
        vec![("position", plan.ends_at.to_string())],
    ));
    match (plan.fork_point, plan.new_generation) {
        (Some(fork), Some(new_generation)) => {
            lines.push(ExplainLine::new(
                "journal-rebuild-forks",
                vec![
                    (
                        "abandoned_generation",
                        fork.abandoned_generation.to_string(),
                    ),
                    ("from_position", fork.from_position.to_string()),
                    ("new_generation", new_generation.to_string()),
                ],
            ));
            lines.push(ExplainLine::new(
                "journal-rebuild-consumers-resync",
                vec![(
                    "abandoned_generation",
                    fork.abandoned_generation.to_string(),
                )],
            ));
        }
        (Some(fork), None) => lines.push(ExplainLine::new(
            "journal-rebuild-abandons-generation",
            vec![
                (
                    "abandoned_generation",
                    fork.abandoned_generation.to_string(),
                ),
                ("from_position", fork.from_position.to_string()),
            ],
        )),
        (None, _) => {
            lines.push(ExplainLine::new(
                "journal-rebuild-keeps-generation",
                Vec::new(),
            ));
            if plan.preserves_positions {
                lines.push(ExplainLine::new(
                    "journal-rebuild-positions-preserved",
                    Vec::new(),
                ));
            }
        }
    }
    if plan.projectors_after {
        lines.push(ExplainLine::new("journal-rebuild-projectors", Vec::new()));
    }
    if plan.invariant_suite_after {
        lines.push(ExplainLine::new(
            "journal-rebuild-invariant-suite",
            Vec::new(),
        ));
    }
    lines
}
