//! The human freeze list: a scope an operator froze is never repaired.

use std::collections::BTreeSet;

use spot_sync_reconcile::Scope;

/// Which scopes of which nodes an operator has frozen.
pub trait FrozenScopes {
    /// Whether `scope` on `node` is frozen.
    fn is_frozen(&self, node: &str, scope: &Scope) -> bool;
}

/// Nothing is frozen.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFrozen;

impl FrozenScopes for NoFrozen {
    fn is_frozen(&self, _node: &str, _scope: &Scope) -> bool {
        false
    }
}

/// An in-memory freeze list.
#[derive(Debug, Clone, Default)]
pub struct MemoryFrozen {
    frozen: BTreeSet<(String, Scope)>,
}

impl MemoryFrozen {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Freezes a scope on a node.
    pub fn freeze(&mut self, node: &str, scope: Scope) {
        self.frozen.insert((node.to_owned(), scope));
    }

    /// Thaws a scope on a node.
    pub fn thaw(&mut self, node: &str, scope: &Scope) {
        self.frozen.remove(&(node.to_owned(), scope.clone()));
    }
}

impl FrozenScopes for MemoryFrozen {
    fn is_frozen(&self, node: &str, scope: &Scope) -> bool {
        self.frozen.contains(&(node.to_owned(), scope.clone()))
    }
}
