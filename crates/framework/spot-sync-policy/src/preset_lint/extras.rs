//! The metadata the preset lints need that `spot-meta` does not carry yet.

use std::collections::{BTreeMap, BTreeSet};

/// Where a unique key's uniqueness comes from (ADR-063 section 2, "Unique natural keys").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyScope {
    /// Unique across the whole network: two nodes can create the same value. **The default.**
    Global,
    /// Unique per origin node (the origin is part of the key).
    OriginScoped,
    /// Generated from a per-node naming series, so two nodes cannot produce the same value.
    PerNodeSeries,
}

/// What the lints ask the metadata layer beyond `EffectiveMeta`. Every method has the
/// conservative default (no flag set), so an implementation overrides only what it knows.
pub trait MetaExtras {
    /// Whether `doctype` is a derived entity (ADR-061): rebuilt at the destination, never synced.
    fn is_derived(&self, _doctype: &str) -> bool {
        false
    }

    /// The closed list of DocTypes the dynamic link `field` of `doctype` may point to
    /// (ADR-064 section 2), or `None` when none is declared.
    fn allowed_targets(&self, _doctype: &str, _field: &str) -> Option<Vec<String>> {
        None
    }

    /// Whether the link `field` of `doctype` is marked `requires_adopted`: the engine refuses
    /// to save the link until the target is adopted (ADR-063 L2).
    fn requires_adopted(&self, _doctype: &str, _field: &str) -> bool {
        false
    }

    /// Where the uniqueness of the unique key `field` of `doctype` comes from.
    fn key_scope(&self, _doctype: &str, _field: &str) -> KeyScope {
        KeyScope::Global
    }

    /// Whether `field` of `doctype` is declared as holding a scope key value, whatever its type.
    fn is_key_typed(&self, _doctype: &str, _field: &str) -> bool {
        false
    }

    /// Whether the author accepts that the target of the link `field` of `doctype` reaches the
    /// nodes of the referrer only by reference (ADR-064 extras) rather than by its own scope.
    fn carried_by_reference(&self, _doctype: &str, _field: &str) -> bool {
        false
    }
}

/// A plain-data [`MetaExtras`], for tests and for callers that read the extras from their own
/// store. Build it with the `with_*` methods.
#[derive(Clone, Debug, Default)]
pub struct PlainExtras {
    derived: BTreeSet<String>,
    allowed: BTreeMap<(String, String), Vec<String>>,
    adopted: BTreeSet<(String, String)>,
    scopes: BTreeMap<(String, String), KeyScope>,
    key_typed: BTreeSet<(String, String)>,
    by_reference: BTreeSet<(String, String)>,
}

fn pair(doctype: &str, field: &str) -> (String, String) {
    (doctype.to_owned(), field.to_owned())
}

impl PlainExtras {
    /// No extras: every method answers with its default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks `doctype` derived.
    #[must_use]
    pub fn with_derived(mut self, doctype: &str) -> Self {
        self.derived.insert(doctype.to_owned());
        self
    }

    /// Declares the `allowed_targets` of a dynamic link.
    #[must_use]
    pub fn with_allowed_targets(mut self, doctype: &str, field: &str, targets: &[&str]) -> Self {
        self.allowed.insert(
            pair(doctype, field),
            targets.iter().map(|t| (*t).to_owned()).collect(),
        );
        self
    }

    /// Marks a link `requires_adopted`.
    #[must_use]
    pub fn with_requires_adopted(mut self, doctype: &str, field: &str) -> Self {
        self.adopted.insert(pair(doctype, field));
        self
    }

    /// Declares where a unique key's uniqueness comes from.
    #[must_use]
    pub fn with_key_scope(mut self, doctype: &str, field: &str, scope: KeyScope) -> Self {
        self.scopes.insert(pair(doctype, field), scope);
        self
    }

    /// Declares a field as holding a scope key value.
    #[must_use]
    pub fn with_key_typed(mut self, doctype: &str, field: &str) -> Self {
        self.key_typed.insert(pair(doctype, field));
        self
    }

    /// Marks a link as carried by reference.
    #[must_use]
    pub fn with_carried_by_reference(mut self, doctype: &str, field: &str) -> Self {
        self.by_reference.insert(pair(doctype, field));
        self
    }
}

impl MetaExtras for PlainExtras {
    fn is_derived(&self, doctype: &str) -> bool {
        self.derived.contains(doctype)
    }

    fn allowed_targets(&self, doctype: &str, field: &str) -> Option<Vec<String>> {
        self.allowed.get(&pair(doctype, field)).cloned()
    }

    fn requires_adopted(&self, doctype: &str, field: &str) -> bool {
        self.adopted.contains(&pair(doctype, field))
    }

    fn key_scope(&self, doctype: &str, field: &str) -> KeyScope {
        self.scopes
            .get(&pair(doctype, field))
            .copied()
            .unwrap_or(KeyScope::Global)
    }

    fn is_key_typed(&self, doctype: &str, field: &str) -> bool {
        self.key_typed.contains(&pair(doctype, field))
    }

    fn carried_by_reference(&self, doctype: &str, field: &str) -> bool {
        self.by_reference.contains(&pair(doctype, field))
    }
}
