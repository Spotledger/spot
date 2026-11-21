//! The health surface: three colours, calm by default, and one short list that needs a person
//! (ADR-067 section 6).

use crate::engine::Open;
use crate::millis::Millis;
use crate::severity::Severity;

/// When an amber condition joins the `needs you` list.
///
/// A tunable setting, never a hidden constant (ADR-067: thresholds stay tunable, and it gives no
/// starting value for this one), so the caller passes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthThresholds {
    /// An amber condition that has been open at least this long needs a person.
    ///
    /// A red condition needs one at once and does not wait for this.
    pub amber_needs_person_after: Millis,
}

impl HealthThresholds {
    /// Thresholds that put an amber condition on the list once it has been open for
    /// `amber_needs_person_after`.
    #[must_use]
    pub const fn new(amber_needs_person_after: Millis) -> Self {
        Self {
            amber_needs_person_after,
        }
    }
}

/// How many conditions are open at each severity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeverityCounts {
    /// Open `info` conditions.
    pub info: usize,
    /// Open `amber` conditions.
    pub amber: usize,
    /// Open `red` conditions.
    pub red: usize,
}

impl SeverityCounts {
    /// Count `open` by severity.
    #[must_use]
    pub fn of(open: &[Open]) -> Self {
        let mut counts = Self::default();
        for condition in open {
            match condition.severity {
                Severity::Info => counts.info += 1,
                Severity::Amber => counts.amber += 1,
                Severity::Red => counts.red += 1,
            }
        }
        counts
    }

    /// The count at `severity`.
    #[must_use]
    pub fn get(&self, severity: Severity) -> usize {
        match severity {
            Severity::Info => self.info,
            Severity::Amber => self.amber,
            Severity::Red => self.red,
        }
    }

    /// Every open condition, at every severity.
    #[must_use]
    pub fn total(&self) -> usize {
        self.info + self.amber + self.red
    }

    /// The worst severity that is open, if any is: the colour of the health surface.
    #[must_use]
    pub fn worst(&self) -> Option<Severity> {
        if self.red > 0 {
            Some(Severity::Red)
        } else if self.amber > 0 {
            Some(Severity::Amber)
        } else if self.info > 0 {
            Some(Severity::Info)
        } else {
            None
        }
    }
}

/// The calm-by-default summary: what is open, and the short list a person has to look at.
///
/// "Calm" is a state the surface can state ([`HealthSummary::is_calm`]), not the absence of news
/// (ADR-067 section 6): an empty `needs_person` list with counts of zero is green, and so is one
/// with only a young amber in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HealthSummary {
    /// How many conditions are open at each severity.
    pub counts: SeverityCounts,
    /// The conditions that need a person: every red one, and every amber one open at least the
    /// threshold. Worst first, then oldest first, so the top of the list is where to start.
    pub needs_person: Vec<Open>,
}

impl HealthSummary {
    /// Summarise `open` at `now` against `thresholds`.
    #[must_use]
    pub fn of(open: &[Open], now: Millis, thresholds: &HealthThresholds) -> Self {
        let mut needs_person: Vec<Open> = open
            .iter()
            .filter(|condition| needs_a_person(condition, now, thresholds))
            .cloned()
            .collect();
        needs_person.sort_by(|left, right| {
            // Worst first, then the one that has been open longest, then a stable order by name
            // so the list never reorders between two calls at the same instant.
            right
                .severity
                .cmp(&left.severity)
                .then_with(|| {
                    now.since(right.raised_at_millis)
                        .cmp(&now.since(left.raised_at_millis))
                })
                .then_with(|| left.id.cmp(&right.id))
                .then_with(|| left.subject.cmp(&right.subject))
        });
        Self {
            counts: SeverityCounts::of(open),
            needs_person,
        }
    }

    /// The colour of the surface: the worst severity open, or `None` for green.
    #[must_use]
    pub fn worst(&self) -> Option<Severity> {
        self.counts.worst()
    }

    /// Whether nothing needs a person now.
    #[must_use]
    pub fn is_calm(&self) -> bool {
        self.needs_person.is_empty()
    }
}

/// Whether `condition` belongs on the `needs you` list at `now`.
fn needs_a_person(condition: &Open, now: Millis, thresholds: &HealthThresholds) -> bool {
    if condition.severity.needs_a_person_at_once() {
        return true;
    }
    condition.severity == Severity::Amber
        && now.elapsed_at_least(
            condition.raised_at_millis,
            thresholds.amber_needs_person_after,
        )
}
