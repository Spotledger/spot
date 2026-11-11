//! The refusals composition can raise: a stable `app_compose.*` code, a plain message and the
//! apps involved.
//!
//! Every problem is data, never a panic: `resolve`, `uninstall` and `upgrade` report **all** the
//! problems of one input at once (sorted, without repeats), the way `spot-seams` reports every
//! [`Conflict`](spot_seams::Conflict). Callers match on the code, never on the message; the
//! catalogue entry for each code lives in `crates/framework/spot-errors/catalogue/app_compose.toml`.

use std::fmt;

/// Stable `app_compose.*` codes. These are the API contract; the messages are not.
pub mod codes {
    /// An input names the same app twice, or the candidate is already installed.
    pub const DUPLICATE_APP: &str = "app_compose.duplicate_app";
    /// Two apps export one interface at the same version.
    pub const DUPLICATE_EXPORT: &str = "app_compose.duplicate_export";
    /// An import has no provider at all, and it is not optional.
    pub const MISSING_PROVIDER: &str = "app_compose.missing_provider";
    /// A provider exists, but none of its versions satisfies the import's range.
    pub const VERSION_CONFLICT: &str = "app_compose.version_conflict";
    /// The compose graph has a cycle: apps extend each other, directly or transitively.
    pub const CYCLE: &str = "app_compose.cycle";
    /// An importing app declares a capability no provider grants to importers.
    pub const CAPABILITY_ESCALATION: &str = "app_compose.capability_escalation";
    /// A provider grants a capability it does not itself hold.
    pub const GRANT_NOT_HELD: &str = "app_compose.grant_not_held";
    /// A composed call used more fuel than the caller's budget allows.
    pub const FUEL_EXHAUSTED: &str = "app_compose.fuel_exhausted";
    /// A provider with dependents cannot be uninstalled while they remain.
    pub const UNINSTALL_HAS_DEPENDENTS: &str = "app_compose.uninstall_has_dependents";
    /// A replacement does not satisfy every dependent's import range.
    pub const REPLACEMENT_INCOMPLETE: &str = "app_compose.replacement_incomplete";
    /// An operation names an app that is not installed.
    pub const UNKNOWN_APP: &str = "app_compose.unknown_app";
    /// An import names an interface no installed app exports, when a provider was expected.
    pub const UNKNOWN_INTERFACE: &str = "app_compose.unknown_interface";
}

/// One reason a composition cannot be resolved, linked or changed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Problem {
    /// A stable machine code, for example `app_compose.missing_provider`.
    pub code: &'static str,
    /// A plain message that names the apps and interfaces involved.
    pub message: String,
    /// The apps involved, sorted and without repeats.
    pub apps: Vec<String>,
}

impl Problem {
    /// Builds a problem from its code, message and the apps involved.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>, apps: &[&str]) -> Self {
        let mut apps: Vec<String> = apps.iter().map(|a| (*a).to_owned()).collect();
        apps.sort();
        apps.dedup();
        Self {
            code,
            message: message.into(),
            apps,
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Every problem found in one input, sorted and without repeats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problems(Vec<Problem>);

impl Problems {
    /// Collects problems, sorting and deduplicating them so the report is deterministic.
    #[must_use]
    pub fn new(mut problems: Vec<Problem>) -> Self {
        problems.sort();
        problems.dedup();
        Self(problems)
    }

    /// The problems, in code order.
    #[must_use]
    pub fn as_slice(&self) -> &[Problem] {
        &self.0
    }

    /// Whether there are no problems.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many problems there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether any problem carries `code`.
    #[must_use]
    pub fn has(&self, code: &str) -> bool {
        self.0.iter().any(|p| p.code == code)
    }
}

impl fmt::Display for Problems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, problem) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Problems {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problems_sort_and_dedup() {
        let a = Problem::new(codes::MISSING_PROVIDER, "b", &["z", "a"]);
        let b = Problem::new(codes::CYCLE, "a", &["m"]);
        let p = Problems::new(vec![a.clone(), b.clone(), a.clone()]);
        assert_eq!(p.len(), 2);
        // Sorted by (code, message, apps): `app_compose.cycle` before `app_compose.missing_provider`.
        assert_eq!(p.as_slice()[0].code, codes::CYCLE);
        assert_eq!(p.as_slice()[1].apps, vec!["a".to_owned(), "z".to_owned()]);
        assert!(p.has(codes::CYCLE));
        assert!(!p.has(codes::DUPLICATE_APP));
        assert!(Problems::new(vec![]).is_empty());
    }
}
