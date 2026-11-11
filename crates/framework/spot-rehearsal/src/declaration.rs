//! What the release metadata declares about a plan.
//!
//! The rehearsal needs two facts the plan alone does not carry: whether a change is a *behaviour*
//! change (so `auto` replay may fire and a fingerprint move may be expected) and whether a change
//! is *high-sensitivity* (tax, valuation, GL posting — a business judgement the platform cannot
//! make). WP-MIG-02's `spot-release-meta` supplies both; this trait is the seam, so a rehearsal
//! works without it and sharpens when it arrives.

use spot_migration_plan::{Area, Change, Risk};

/// What the release metadata declares about a plan.
pub trait ReleaseDeclaration: Send + Sync {
    /// Whether `change` is a declared behaviour change (it may move the fingerprint).
    fn is_behaviour_change(&self, change: &Change) -> bool;

    /// Whether `change` is high-sensitivity (the site wants replay for it).
    fn is_high_sensitivity(&self, change: &Change) -> bool;

    /// The fingerprint section keys `change` declares it may move. Empty means it declares no
    /// fingerprint impact, so a difference in any section is unexplained.
    fn expected_sections(&self, change: &Change) -> Vec<String>;
}

/// A release that declares nothing: no behaviour change, nothing high-sensitivity, no expected
/// fingerprint impact. A rehearsal with this declaration treats every difference as unexplained.
pub struct NoDeclaration;

impl ReleaseDeclaration for NoDeclaration {
    fn is_behaviour_change(&self, _change: &Change) -> bool {
        false
    }

    fn is_high_sensitivity(&self, _change: &Change) -> bool {
        false
    }

    fn expected_sections(&self, _change: &Change) -> Vec<String> {
        Vec::new()
    }
}

/// A declaration derived from the plan itself, for when WP-MIG-02 is not wired in: a change in the
/// behaviour area is a behaviour change, and a destructive change is high-sensitivity. It declares
/// no expected fingerprint section — only the release metadata knows which balance a change moves.
pub struct PlanDeclaration;

impl ReleaseDeclaration for PlanDeclaration {
    fn is_behaviour_change(&self, change: &Change) -> bool {
        change.area == Area::Behaviour
    }

    fn is_high_sensitivity(&self, change: &Change) -> bool {
        change.risk == Risk::Destructive
    }

    fn expected_sections(&self, _change: &Change) -> Vec<String> {
        Vec::new()
    }
}
