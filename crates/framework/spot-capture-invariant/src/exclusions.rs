//! The explicit exclusions of ADR-061 s3: derived and local-only entities. Every exclusion has
//! an owner and a reason, and the report lists each one with how much it skipped.

use std::collections::HashMap;

use serde::Serialize;

use crate::error::CaptureInvariantError;

/// Why an entity is outside the captured record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionKind {
    /// Rebuilt by a projector after a rebuild (balances, caches, indexes, the analytics mirror).
    Derived,
    /// Local-only by its sync policy; the owner states it is not part of the recoverable record.
    Local,
}

/// One declared exclusion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Exclusion {
    /// The excluded entity.
    pub entity: String,
    /// Derived or local-only.
    pub kind: ExclusionKind,
    /// Who excluded it (only the owner can).
    pub owner: String,
    /// Why, in the owner's words.
    pub reason: String,
    /// For a derived entity, the registered projector name; `None` is a catalog error.
    pub projector: Option<String>,
}

/// The set of exclusions, one per entity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exclusions {
    items: Vec<Exclusion>,
    index: HashMap<String, usize>,
}

impl Exclusions {
    /// No exclusions: everything is captured.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a derived entity. `projector` is the registered projector's name; pass `None`
    /// when the catalog names none, and the checker reports `DerivedWithoutProjector`.
    ///
    /// # Errors
    ///
    /// `capture_invariant.exclusion_invalid` for an empty entity, owner or reason, or an entity
    /// already declared.
    pub fn derived(
        self,
        entity: &str,
        owner: &str,
        reason: &str,
        projector: Option<&str>,
    ) -> Result<Self, CaptureInvariantError> {
        self.add(entity, ExclusionKind::Derived, owner, reason, projector)
    }

    /// Declare a local-only entity.
    ///
    /// # Errors
    ///
    /// As [`Exclusions::derived`].
    pub fn local(
        self,
        entity: &str,
        owner: &str,
        reason: &str,
    ) -> Result<Self, CaptureInvariantError> {
        self.add(entity, ExclusionKind::Local, owner, reason, None)
    }

    fn add(
        mut self,
        entity: &str,
        kind: ExclusionKind,
        owner: &str,
        reason: &str,
        projector: Option<&str>,
    ) -> Result<Self, CaptureInvariantError> {
        if entity.trim().is_empty() {
            return Err(CaptureInvariantError::ExclusionInvalid("entity is empty"));
        }
        if owner.trim().is_empty() {
            return Err(CaptureInvariantError::ExclusionInvalid("owner is empty"));
        }
        if reason.trim().is_empty() {
            return Err(CaptureInvariantError::ExclusionInvalid("reason is empty"));
        }
        if self.index.contains_key(entity) {
            return Err(CaptureInvariantError::ExclusionInvalid(
                "entity is declared twice",
            ));
        }
        let projector = projector
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        self.index.insert(entity.to_owned(), self.items.len());
        self.items.push(Exclusion {
            entity: entity.to_owned(),
            kind,
            owner: owner.to_owned(),
            reason: reason.to_owned(),
            projector,
        });
        Ok(self)
    }

    /// The declared exclusions in declaration order.
    #[must_use]
    pub fn items(&self) -> &[Exclusion] {
        &self.items
    }

    pub(crate) fn position_of(&self, entity: &str) -> Option<usize> {
        self.index.get(entity).copied()
    }
}
