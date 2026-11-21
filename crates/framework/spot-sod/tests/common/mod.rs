//! Shared fixtures: the seeded rule set (tests/rules.toml, loaded through the public
//! loader so the loader is exercised on every run), the clean reference catalogue
//! (tests/roles.toml), and assignment builders.

#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_sod::{Assignment, RoleCatalogue, RuleSet};

/// The seeded rule set, loaded from `rules.toml` in the tests directory (the common module
/// lives one level down, hence the `../`).
pub fn rules() -> RuleSet {
    let text = include_str!("../rules.toml");
    RuleSet::from_toml(text).expect("the seeded rules load")
}

/// The clean reference catalogue: `tests/roles.toml` (one level up) **without** its last
/// entry (the Workday-finding role, which the file carries so `internal_conflicts` has a
/// subject). The delivered reference roles must have no internal conflict, so the shipped
/// pack is this one minus the finding.
pub fn clean_catalogue() -> RoleCatalogue {
    let text = include_str!("../roles.toml");
    let mut catalogue = RoleCatalogue::from_toml(text).expect("the seeded catalogue loads");
    catalogue
        .roles
        .remove(&spot_sod::Role::new("Conflict Role").expect("role"));
    catalogue
}

/// A catalogue carrying the Workday finding: one role holding both sides of two rules.
pub fn finding_catalogue() -> RoleCatalogue {
    let text = r#"
[[roles]]
name = "Super Clerk"
duties = ["Maintain supplier master", "Approve payment run"]

[[roles]]
name = "Ledger Accountant"
duties = ["Reconcile ledger"]
"#;
    RoleCatalogue::from_toml(text).expect("the finding catalogue loads")
}

/// A direct duty assignment.
pub fn duty(user: &str, duty_text: &str, scope: Option<&str>) -> Assignment {
    Assignment::duty(user, duty_text, scope)
}

/// A role assignment.
pub fn role(user: &str, role_text: &str, scope: Option<&str>) -> Assignment {
    Assignment::role(user, role_text, scope)
}
