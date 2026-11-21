//! The lint flag for the old and the new sync vocabulary (WP-SYN-01.T6, decision C6 option a).
//!
//! Both vocabularies stay accepted behind one flag, [`PolicyMode`]. Under
//! [`PolicyMode::Both`] (the default) a declaration in the old words is a warning; under
//! [`PolicyMode::NewOnly`] it is an error. Either way the finding carries the proposed rewrite
//! text, or the questions a person must answer ([`rewrite_candidate`]).
//!
//! The existing entry points ([`crate::lint`], [`crate::verify`]) are unchanged; they report
//! only errors. [`lint_vocabulary`] and [`verify_with_mode`] add the flag and return a
//! [`Report`] that keeps errors and warnings apart. Nothing here reads a clock or an
//! environment variable: the mode is an argument.

mod rewrite;

use crate::hook::unknown_doctype_problems;
use crate::lint::{PolicyProblem, lint};
use crate::policy::{SyncPolicy, Trigger};
use crate::table::{PolicySet, PolicyTable, resolve};
use spot_meta::EffectiveMeta;

pub use rewrite::{Refusal, Rewrite, rewrite_candidate, rewrite_candidate_with};

/// The stable dotted codes of the vocabulary lint. Each is catalogued in `sync_policy.toml`.
pub mod codes {
    /// A DocType declares its policy in the retired vocabulary.
    pub const RETIRED_VOCABULARY: &str = "sync_policy.retired_vocabulary";
    /// A policy sets a trigger, which nothing implements (decision C5).
    pub const RETIRED_TRIGGER: &str = "sync_policy.retired_trigger";
}

/// Which vocabulary the lint accepts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PolicyMode {
    /// Both vocabularies are accepted; the old words are a warning. **The default.**
    #[default]
    Both,
    /// Only the new vocabulary is accepted; the old words are an error.
    NewOnly,
}

/// What the lint found, errors and warnings apart. Both lists keep a fixed order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Findings that refuse the declaration.
    pub errors: Vec<PolicyProblem>,
    /// Findings that are shown and do not refuse.
    pub warnings: Vec<PolicyProblem>,
}

impl Report {
    /// Whether nothing was found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty() && self.warnings.is_empty()
    }
}

/// The vocabulary findings for every policy `declared`, in DocType order. A policy in `declared`
/// is a declaration in the old words, whatever its value.
#[must_use]
pub fn lint_vocabulary(declared: &PolicySet, mode: PolicyMode) -> Report {
    let mut report = Report::default();
    for doctype in declared.doctypes() {
        let Some(old) = declared.get(doctype) else {
            continue;
        };
        let finding = PolicyProblem::new(codes::RETIRED_VOCABULARY, doctype, detail(old));
        match mode {
            PolicyMode::Both => report.warnings.push(finding),
            PolicyMode::NewOnly => report.errors.push(finding),
        }
        if old.trigger != Trigger::OnSubmit {
            report.warnings.push(PolicyProblem::new(
                codes::RETIRED_TRIGGER,
                doctype,
                format!(
                    "trigger `{}` is accepted and ignored: implemented by nothing",
                    old.trigger
                ),
            ));
        }
    }
    report
}

fn detail(old: &SyncPolicy) -> String {
    let owner = old
        .owner_scope
        .as_ref()
        .map_or_else(String::new, |k| format!(", owner scope `{k}`"));
    let words = format!(
        "declares the retired vocabulary (class `{}`, direction `{}`{owner})",
        old.class, old.direction
    );
    match rewrite_candidate(old) {
        Rewrite::Mechanical(new) => {
            format!("{words}; declare instead:\n{}", new.to_text().trim_end())
        }
        Rewrite::NeedsHuman { reasons } => {
            let mut text = format!("{words}; it needs a person, nothing is rewritten:");
            for reason in &reasons {
                text.push_str("\n- ");
                text.push_str(&reason.question());
                if let Refusal::SharedBoth { candidates, .. } = reason {
                    for candidate in candidates {
                        text.push_str("\n  candidate:\n  ");
                        text.push_str(&candidate.replace('\n', "\n  "));
                    }
                }
            }
            text
        }
    }
}

/// As [`crate::verify`], with the flag. Resolves `declared` over `meta`, runs the policy lints
/// and the vocabulary lint, and returns the table with every error and warning at once. A
/// non-empty [`Report::errors`] means the metadata must not go live.
#[must_use]
pub fn verify_with_mode(
    meta: &EffectiveMeta,
    declared: &PolicySet,
    mode: PolicyMode,
) -> (PolicyTable, Report) {
    let table = resolve(meta, declared);
    let mut report = lint_vocabulary(declared, mode);
    let mut errors = unknown_doctype_problems(&table);
    errors.extend(lint(&table, meta));
    errors.append(&mut report.errors);
    report.errors = errors;
    (table, report)
}
