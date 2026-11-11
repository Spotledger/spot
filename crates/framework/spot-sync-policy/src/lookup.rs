//! The seam the other sync crates consult: [`PolicyLookup`].
//!
//! `spot-partition` (ownership and naming) and `spot-sync-master` (master-data propagation) need
//! to know a DocType's policy — is it synced, which way may it flow, does it name an owner scope
//! — without depending on this crate's internals. They take a `&dyn PolicyLookup` (or a generic
//! `P: PolicyLookup`) and ask. [`PolicyTable`] implements it; [`StaticPolicies`] is the in-memory
//! one a test or a single-node start uses.

use std::collections::BTreeMap;

use crate::policy::{Direction, ScopeKeyType, SyncPolicy};
use crate::table::PolicyTable;

/// A source of resolved sync policy, by DocType name.
///
/// A DocType the source does not hold is treated as `local`: the default policy, which syncs
/// nothing. The default methods are the ones a consumer wants; only [`PolicyLookup::policy`] must
/// be implemented.
pub trait PolicyLookup {
    /// The policy of a DocType, or `None` when the source does not hold it.
    fn policy(&self, doctype: &str) -> Option<&SyncPolicy>;

    /// Whether the DocType syncs at all.
    fn is_synced(&self, doctype: &str) -> bool {
        self.policy(doctype).is_some_and(SyncPolicy::is_synced)
    }

    /// The DocType's direction; `none` when it is not held.
    fn direction(&self, doctype: &str) -> Direction {
        self.policy(doctype)
            .map_or(Direction::None, |p| p.direction)
    }

    /// The DocType's owner scope key type, when it names one.
    fn owner_scope(&self, doctype: &str) -> Option<&ScopeKeyType> {
        self.policy(doctype).and_then(|p| p.owner_scope.as_ref())
    }

    /// Whether a record of the DocType may travel `direction`. `both` allows `up` and `down`;
    /// `none` allows nothing.
    fn allows(&self, doctype: &str, direction: Direction) -> bool {
        match (self.direction(doctype), direction) {
            (Direction::Both, Direction::Up | Direction::Down) => true,
            (a, b) => a == b,
        }
    }
}

impl PolicyLookup for PolicyTable {
    fn policy(&self, doctype: &str) -> Option<&SyncPolicy> {
        PolicyTable::get(self, doctype)
    }
}

/// A fixed set of policies, for a test or a single-node start that has no resolved table.
#[derive(Clone, Debug, Default)]
pub struct StaticPolicies {
    policies: BTreeMap<String, SyncPolicy>,
}

impl StaticPolicies {
    /// An empty set: every DocType is `local`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces one DocType's policy.
    pub fn insert(&mut self, doctype: &str, policy: SyncPolicy) {
        self.policies.insert(doctype.to_owned(), policy);
    }

    /// One DocType's policy, when set.
    #[must_use]
    pub fn get(&self, doctype: &str) -> Option<&SyncPolicy> {
        self.policies.get(doctype)
    }
}

impl PolicyLookup for StaticPolicies {
    fn policy(&self, doctype: &str) -> Option<&SyncPolicy> {
        self.policies.get(doctype)
    }
}
