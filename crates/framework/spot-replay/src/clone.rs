//! The clone a replay may write to.
//!
//! A [`CloneRef`] addresses the scratch copy of the site that WP-MIG-06.T1's rehearsal created
//! from a snapshot. It is deliberately **not** an address of the live site: a [`ReplayRunner`]
//! only ever receives a `CloneRef`, so the "replay never writes outside the clone" rule is held
//! by the type, not by convention. A host that holds T1's
//! [`CloneTarget`](spot_rehearsal::CloneTarget) builds a `CloneRef` from its site and its scratch
//! label.

use serde::{Deserialize, Serialize};

/// Where a replay's clone lives: the site it is a copy of, and the scratch's label.
///
/// The label is a database name or a data-directory path — never a secret, and never a
/// connection string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneRef {
    site: String,
    label: String,
}

impl CloneRef {
    /// A clone for `site` with the scratch `label`.
    #[must_use]
    pub fn new(site: impl Into<String>, label: impl Into<String>) -> Self {
        CloneRef {
            site: site.into(),
            label: label.into(),
        }
    }

    /// The site the clone is a copy of.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    /// The scratch's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_keeps_its_site_and_label() {
        let clone = CloneRef::new("acme", "spot_scratch");
        assert_eq!(clone.site(), "acme");
        assert_eq!(clone.label(), "spot_scratch");
    }
}
