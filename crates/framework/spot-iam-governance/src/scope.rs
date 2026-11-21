//! The scope a grant carries, and the scope hierarchy that resolves "including descendants"
//! (ADR-070 section 3 rule 2 and section 11 rule 1).
//!
//! A [`GrantScope`] is the value restriction of one grant: the link values it applies to,
//! optionally including their descendants. The platform stores scope as opaque link values
//! (ADR-058 rule 2); [`ScopeHierarchy`] is the caller-supplied parent/child relation the
//! "include descendants" option needs, never knowledge this crate invents.
//!
//! The segregation-of-duties decision core (`spot-sod`) reasons about a single scope token per
//! assignment. A grant that covers several values (or a value and its descendants) is
//! **expanded** into one token per covered value, so two grants conflict exactly when they
//! share a value, whatever combination of plain and descendant scopes produced them.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use spot_iam_roles::Scope;
use spot_permissions::{AppliesTo, UserPermission};

/// The value restriction of one grant: the link values it applies to, and whether it also
/// reaches their descendants.
///
/// An empty `values` set is an **unscoped** grant: the role applies everywhere. It covers
/// every other scope; no scoped grant covers an unscoped one.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct GrantScope {
    /// The link field or link DocType the values belong to; informational for the SoD check
    /// and preserved when the scope becomes an assignment's [`Scope`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimension: Option<String>,
    /// The allowed link values (record-id texts, or select values). Empty means everywhere.
    #[serde(default)]
    pub values: BTreeSet<String>,
    /// Whether the scope also reaches the descendants of `values`.
    #[serde(default)]
    pub include_descendants: bool,
}

impl GrantScope {
    /// The unscoped grant: the role applies everywhere.
    #[must_use]
    pub fn unscoped() -> GrantScope {
        GrantScope::default()
    }

    /// A grant restricted to one value of one dimension.
    #[must_use]
    pub fn value(dimension: &str, value: &str) -> GrantScope {
        let mut values = BTreeSet::new();
        values.insert(value.to_owned());
        GrantScope {
            dimension: Some(dimension.to_owned()),
            values,
            include_descendants: false,
        }
    }

    /// The same scope, also reaching the descendants of its values.
    #[must_use]
    pub fn with_descendants(mut self) -> GrantScope {
        self.include_descendants = true;
        self
    }

    /// Whether the grant applies everywhere.
    #[must_use]
    pub fn is_unscoped(&self) -> bool {
        self.values.is_empty()
    }

    /// The scope tokens the grant applies in, for the SoD check: one per covered value, plus
    /// the descendants of each value when [`Self::include_descendants`] is set. An unscoped
    /// grant has the single token `None`.
    #[must_use]
    pub fn tokens(&self, hierarchy: &ScopeHierarchy) -> BTreeSet<Option<String>> {
        if self.values.is_empty() {
            let mut out = BTreeSet::new();
            out.insert(None);
            return out;
        }
        let mut out = BTreeSet::new();
        for value in &self.values {
            out.insert(Some(value.clone()));
            if self.include_descendants {
                for child in hierarchy.descendants(value) {
                    out.insert(Some(child));
                }
            }
        }
        out
    }

    /// Whether this scope covers `other`: every token of `other` is a token of this scope.
    ///
    /// An unscoped scope covers everything; a scoped scope never covers an unscoped one.
    #[must_use]
    pub fn covers(&self, other: &GrantScope, hierarchy: &ScopeHierarchy) -> bool {
        if self.is_unscoped() {
            return true;
        }
        if other.is_unscoped() {
            return false;
        }
        let mine = self.tokens(hierarchy);
        other.tokens(hierarchy).iter().all(|t| mine.contains(t))
    }

    /// The grant scope an assignment's [`Scope`] carries: the union of every restriction's
    /// allowed values, with descendants if any restriction asks for them.
    ///
    /// The SoD check reasons about *which values* two grants share; a grant whose restrictions
    /// name several values applies in all of them.
    #[must_use]
    pub fn from_scope(scope: &Scope) -> GrantScope {
        let mut out = GrantScope::default();
        for restriction in &scope.restrictions {
            for value in &restriction.allowed {
                out.values.insert(value.clone());
            }
            out.include_descendants |= restriction.include_descendants;
            if out.dimension.is_none() && !restriction.field_or_link_doctype.is_empty() {
                out.dimension = Some(restriction.field_or_link_doctype.clone());
            }
        }
        out
    }

    /// The assignment [`Scope`] this grant scope becomes: one restriction over the values, or
    /// no restriction when the grant is unscoped.
    #[must_use]
    pub fn to_scope(&self) -> Scope {
        if self.values.is_empty() {
            return Scope::default();
        }
        Scope {
            restrictions: vec![UserPermission {
                user: String::new(),
                doctype: None,
                field_or_link_doctype: self.dimension.clone().unwrap_or_default(),
                allowed: self.values.iter().cloned().collect(),
                applies_to: AppliesTo::AllDoctypes,
                include_descendants: self.include_descendants,
            }],
        }
    }
}

/// The caller-supplied parent/child relation over scope values, for the "include descendants"
/// option.
///
/// The platform does not know what a value is (ADR-058 rule 2); the ERP layer supplies the
/// organisation hierarchy. A hierarchy is acyclic by construction here: [`ScopeHierarchy::add`]
/// only records an edge, and [`ScopeHierarchy::descendants`] walks with a visited set, so a
/// caller that feeds a cycle gets a terminating answer, not a hang.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScopeHierarchy {
    children: BTreeMap<String, BTreeSet<String>>,
}

impl ScopeHierarchy {
    /// An empty hierarchy: every value is its own only scope.
    #[must_use]
    pub fn new() -> ScopeHierarchy {
        ScopeHierarchy::default()
    }

    /// Records that `child` is a descendant of `parent`.
    pub fn add(&mut self, parent: &str, child: &str) {
        if parent == child {
            return;
        }
        self.children
            .entry(parent.to_owned())
            .or_default()
            .insert(child.to_owned());
    }

    /// Builds a hierarchy from `(parent, child)` edges.
    #[must_use]
    pub fn from_edges(edges: impl IntoIterator<Item = (String, String)>) -> ScopeHierarchy {
        let mut out = ScopeHierarchy::new();
        for (parent, child) in edges {
            out.add(&parent, &child);
        }
        out
    }

    /// Every descendant of `value`, transitively, sorted and deduplicated.
    #[must_use]
    pub fn descendants(&self, value: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut frontier: Vec<String> = self
            .children
            .get(value)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        while let Some(next) = frontier.pop() {
            if !out.insert(next.clone()) {
                continue;
            }
            if let Some(grand) = self.children.get(&next) {
                frontier.extend(grand.iter().cloned());
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn an_unscoped_scope_has_the_single_none_token_and_covers_everything() {
        let h = ScopeHierarchy::new();
        let unscoped = GrantScope::unscoped();
        assert!(unscoped.is_unscoped());
        assert_eq!(unscoped.tokens(&h), BTreeSet::from([None]));
        assert!(unscoped.covers(&GrantScope::value("Company", "A"), &h));
        assert!(!GrantScope::value("Company", "A").covers(&unscoped, &h));
    }

    #[test]
    fn descendants_expand_a_scope_into_one_token_per_covered_value() {
        let mut h = ScopeHierarchy::new();
        h.add("A", "A/div1");
        h.add("A/div1", "A/div1/team");
        let scope = GrantScope::value("Company", "A").with_descendants();
        let tokens = scope.tokens(&h);
        assert_eq!(
            tokens,
            BTreeSet::from([
                Some("A".to_owned()),
                Some("A/div1".to_owned()),
                Some("A/div1/team".to_owned()),
            ])
        );
        // A plain child scope is covered by the descendant scope.
        assert!(scope.covers(&GrantScope::value("Company", "A/div1"), &h));
        assert!(scope.covers(&GrantScope::value("Company", "A/div1/team"), &h));
        assert!(!scope.covers(&GrantScope::value("Company", "B"), &h));
    }

    #[test]
    fn a_cycle_terminates() {
        let mut h = ScopeHierarchy::new();
        h.add("A", "B");
        h.add("B", "A");
        assert_eq!(
            h.descendants("A"),
            BTreeSet::from(["A".to_owned(), "B".to_owned()])
        );
    }
}
