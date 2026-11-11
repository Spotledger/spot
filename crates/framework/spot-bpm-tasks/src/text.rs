//! User-visible text and the run-label pair (ADR-080).
//!
//! Users never see the internal word for a run. Every string that names one goes through a
//! [`RunLabel`], whose shipped defaults come from the model's `bpm.run.singular` /
//! `bpm.run.plural` keys ("Request" / "Requests"). An organisation may override either label,
//! and [`user_visible_strings`] enumerates every string this crate can show, so a test can prove
//! none of them names a run with the internal word.

use spot_bpm_model::model::labels::{RUN_PLURAL_DEFAULT, RUN_SINGULAR_DEFAULT};

use crate::notify::NotificationKind;

/// The user-facing name of a run: a singular and a plural label (ADR-080).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunLabel {
    /// One run, e.g. "Request".
    pub singular: String,
    /// Several runs, e.g. "Requests".
    pub plural: String,
}

impl Default for RunLabel {
    fn default() -> Self {
        Self::shipped()
    }
}

impl RunLabel {
    /// The shipped defaults from the model's `bpm.run.*` keys.
    #[must_use]
    pub fn shipped() -> Self {
        RunLabel {
            singular: RUN_SINGULAR_DEFAULT.to_owned(),
            plural: RUN_PLURAL_DEFAULT.to_owned(),
        }
    }

    /// The labels with organisation overrides; an empty or blank override falls back to the
    /// shipped default, so a bad setting never produces an empty name.
    #[must_use]
    pub fn with_overrides(singular: Option<&str>, plural: Option<&str>) -> Self {
        let shipped = Self::shipped();
        RunLabel {
            singular: clean(singular).unwrap_or(shipped.singular),
            plural: clean(plural).unwrap_or(shipped.plural),
        }
    }
}

/// A trimmed, non-empty override, or `None`.
fn clean(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The subject line for a notification kind, from the run label.
#[must_use]
pub fn notification_subject(kind: NotificationKind, run_singular: &str) -> String {
    match kind {
        NotificationKind::Assigned => {
            format!("A {run_singular} is waiting for your decision")
        }
        NotificationKind::Reminder => format!("Reminder: a {run_singular} is still waiting"),
        NotificationKind::Escalated => format!("Escalated: a {run_singular} needs your decision"),
        NotificationKind::Delegated => format!("A {run_singular} was delegated to you"),
        NotificationKind::Reassigned => format!("A {run_singular} was reassigned to you"),
        NotificationKind::Completed => format!("A {run_singular} was decided"),
        NotificationKind::AutoApproved => {
            format!("A {run_singular} was approved automatically by its deadline")
        }
    }
}

/// Every user-visible string this crate can produce, for a given label pair.
///
/// The inbox headings, the empty-inbox notes and every notification subject are all here, so a
/// test can scan them for a forbidden word without having to drive the whole service.
#[must_use]
pub fn user_visible_strings(label: &RunLabel) -> Vec<String> {
    let mut strings = vec![
        format!("My {}", label.plural),
        format!("My team's {}", label.plural),
        format!("{} by process", label.plural),
        format!("Overdue {}", label.plural),
        format!("Start a {}", label.singular),
        format!("No {} to decide", label.plural),
        format!("Decide this {}", label.singular),
        format!("This {} was already decided", label.singular),
        format!("{} is waiting for a decision", label.singular),
    ];
    for kind in [
        NotificationKind::Assigned,
        NotificationKind::Reminder,
        NotificationKind::Escalated,
        NotificationKind::Delegated,
        NotificationKind::Reassigned,
        NotificationKind::Completed,
        NotificationKind::AutoApproved,
    ] {
        strings.push(notification_subject(kind, &label.singular));
    }
    strings
}
